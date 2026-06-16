// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/global_resolved_layers.h"

#include <lib/syslog/cpp/macros.h>

namespace flatland {

void ComputeGlobalResolvedLayers(std::vector<ResolvedLayer>& output,
                                 const std::vector<ImageRect>& rectangles,
                                 const std::vector<allocation::ImageMetadata>& images) {
  FX_DCHECK(rectangles.size() == images.size());
  output.clear();
  output.reserve(rectangles.size());
  for (size_t i = 0; i < rectangles.size(); ++i) {
    const auto& rect = rectangles[i];
    const auto& meta = images[i];
    ResolvedLayer layer;
    layer.rect = rect;
    layer.blend_mode = meta.blend_mode;
    layer.flip = meta.flip;

    if (meta.identifier == allocation::kInvalidImageId) {
      // TODO(https://fxbug.dev/523371761): currently, the opacity is already pre-baked into
      // meta.multiply_color.  Eventually, there will be no `ComputeGlobalImageData()` function,
      // and opacity will be handled directly in this function (the signature will change to
      // include opacity data).
      layer.color = {1.f, 1.f, 1.f, 1.f};
      layer.content = ResolvedLayer::SolidColorContent{.color = meta.multiply_color};
    } else {
      layer.color = meta.multiply_color;
      layer.content = ResolvedLayer::ImageContent{
          .image_id = meta.identifier,
          .width = meta.width,
          .height = meta.height,
      };
    }
    output.push_back(std::move(layer));
  }
}

}  // namespace flatland
