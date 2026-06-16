// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_GLOBAL_RESOLVED_LAYERS_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_GLOBAL_RESOLVED_LAYERS_H_

#include <vector>

#include "src/ui/scenic/lib/allocation/image_metadata.h"
#include "src/ui/scenic/lib/flatland/flatland_types.h"

namespace flatland {

// Zips the legacy pipeline's parallel outputs into ResolvedLayers.
// |rectangles| and |images| must be the same length (the existing RenderData
// invariant).  An entry whose metadata.identifier == kInvalidImageId becomes
// SolidColorContent{multiply_color}; all others become ImageContent.
void ComputeGlobalResolvedLayers(std::vector<ResolvedLayer>& output,
                                 const std::vector<ImageRect>& rectangles,
                                 const std::vector<allocation::ImageMetadata>& images);

inline std::vector<ResolvedLayer> ComputeGlobalResolvedLayers(
    const std::vector<ImageRect>& rectangles,
    const std::vector<allocation::ImageMetadata>& images) {
  std::vector<ResolvedLayer> output;
  ComputeGlobalResolvedLayers(output, rectangles, images);
  return output;
}

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_GLOBAL_RESOLVED_LAYERS_H_
