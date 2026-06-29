// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND1_STATE_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND1_STATE_H_

#include <unordered_map>

#include "src/ui/scenic/lib/allocation/image_metadata.h"
#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/transform_handle.h"

namespace flatland {

// Session-side state for the classic (Flatland1) content representation.
// TODO(https://fxbug.dev/523371761): delete completely once the facade is the only content path
// (facade step 130).
struct Flatland1ContentState {
  // A mapping from Flatland-generated TransformHandle to the ImageMetadata it
  // represents. Filled rects are stored here too, with identifier == kInvalidImageId.
  std::unordered_map<TransformHandle, allocation::ImageMetadata> image_metadatas;

  // TODO(https://fxbug.dev/523371761): entries are never erased (no cleanup in
  // `ProcessDeadTransforms()` or elsewhere).  This is a long-standing, bounded leak which is
  // intentionally not fixed because this state will bee deleted soon (Flatland1 facade step 130).
  // The v2 replacement avoids the class of bug by construction - see flatland2_state.h.
  std::unordered_map<TransformHandle, ImageSampleRegion> image_sample_regions;

  // Shared by the ~10 SetImage*/ReleaseImage* methods: returns the metadata for
  // |content_handle|, or nullptr if it is not image content. Callers keep their
  // own (distinct) error messages - this only collapses the find/end-check.
  allocation::ImageMetadata* FindImage(TransformHandle content_handle);
  const allocation::ImageMetadata* FindImage(TransformHandle content_handle) const;
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND1_STATE_H_
