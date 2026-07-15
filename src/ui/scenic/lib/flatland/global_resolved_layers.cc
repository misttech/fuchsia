// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/global_resolved_layers.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>

#include <algorithm>

namespace flatland {

void ComputeGlobalResolvedLayers(std::vector<ResolvedLayer>& output,
                                 const std::vector<ImageRect>& rectangles,
                                 const std::vector<allocation::ImageMetadata>& images,
                                 const std::vector<size_t>& image_indices) {
  FX_DCHECK(rectangles.size() == images.size());
  FX_DCHECK(image_indices.empty() || image_indices.size() == rectangles.size());
  output.clear();
  output.reserve(rectangles.size());
  for (size_t i = 0; i < rectangles.size(); ++i) {
    const auto& rect = rectangles[i];
    const auto& meta = images[i];
    ResolvedLayer layer;
    layer.rect = rect;
    layer.blend_mode = meta.blend_mode;
    layer.flip = meta.flip;
    layer.topology_index = image_indices.empty() ? ResolvedLayer::kInvalidTopologyIndex
                                                 : static_cast<int32_t>(image_indices[i]);

    if (meta.identifier == allocation::kInvalidImageId) {
      // TODO(https://fxbug.dev/523371761): currently, the opacity is already pre-baked into
      // meta.multiply_color.  Eventually, there will be no `ComputeGlobalImageData()` function,
      // and opacity will be handled directly in this function (the signature will change to
      // include opacity data).
      layer.multiply_color = {1.f, 1.f, 1.f, 1.f};
      layer.content = ResolvedLayer::SolidColorContent{.color = meta.multiply_color};
    } else {
      layer.multiply_color = meta.multiply_color;
      layer.content = ResolvedLayer::ImageContent{
          .image_id = meta.identifier,
          .width = meta.width,
          .height = meta.height,
      };
    }
    output.push_back(std::move(layer));
  }
}

void CullLayersInPlace(std::vector<flatland::ResolvedLayer>* layers_in_out, uint64_t display_width,
                       uint64_t display_height) {
  TRACE_DURATION("gfx", "CullLayersInPlace");
  FX_DCHECK(layers_in_out);
  auto is_occluder = [display_width, display_height](const flatland::ResolvedLayer& layer) -> bool {
    // Only cull if the rect is opaque.
    auto is_opaque = layer.blend_mode == flatland::BlendMode::kReplace();

    // If the rect is full screen (or larger), and opaque, clear the output vectors.
    return (is_opaque && layer.rect.origin.x <= 0 && layer.rect.origin.y <= 0 &&
            layer.rect.extent.x >= static_cast<float>(display_width) &&
            layer.rect.extent.y >= static_cast<float>(display_height));
  };

  // Find the index of the last occluder.
  size_t occluder_index = 0;
  for (size_t i = 0; i < layers_in_out->size(); i++) {
    if (is_occluder((*layers_in_out)[i])) {
      occluder_index = i;
    }
  }

  // Move all of the remaining renderable data into the output vectors. Entries get erased
  // if they occur before the last occluder index, or if the rectangle at that entry is empty.
  const auto is_rect_empty = [](const flatland::ImageRect& rect) {
    return rect.extent.x <= 0.f || rect.extent.y <= 0.f;
  };

  layers_in_out->erase(
      std::remove_if(layers_in_out->begin(), layers_in_out->end(),
                     [index = static_cast<size_t>(0), occluder_index,
                      &is_rect_empty](const flatland::ResolvedLayer& layer) mutable {
                       auto curr_index = index++;
                       return curr_index < occluder_index || is_rect_empty(layer.rect);
                     }),
      layers_in_out->end());
}

}  // namespace flatland
