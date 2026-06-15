// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_UBER_STRUCT_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_UBER_STRUCT_H_

#include <memory>
#include <memory_resource>
#include <string>
#include <unordered_map>

#include "src/ui/scenic/lib/allocation/image_metadata.h"
#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/transform_graph.h"
#include "src/ui/scenic/lib/flatland/transform_handle.h"

#include <glm/glm.hpp>
#include <glm/mat3x3.hpp>

namespace flatland {

// TODO(https://fxbug.dev/42122511): find the appropriate name for this struct.
//
// A collection of data local to a particular Flatland instance representing the most recent commit
// of that instance's presented state. Because the UberStruct represents a snapshot of the local
// state of a Flatland instance, it must be stateless. It should contain only data and no
// references to external resources.
struct UberStruct {
 private:
  // The memory resource used for allocations in this struct. Must be declared first
  // so it is destroyed last.
  std::pmr::monotonic_buffer_resource resource_;

 public:
  using InstanceMap =
      std::unordered_map<TransformHandle::InstanceId, std::shared_ptr<const UberStruct>>;

  UberStruct()
      : local_topology(&resource_),
        local_matrices(&resource_),
        local_opacity_values(&resource_),
        local_image_sample_regions(&resource_),
        local_clip_regions(&resource_),
        images(&resource_),
        local_hit_regions_map(&resource_),
        debug_name(&resource_) {}

  // Note: this MUST only be used to allocate memory for this UberStruct's fields
  std::pmr::memory_resource* resource() { return &resource_; }

  // The local topology of this Flatland instance.
  TransformGraph::TopologyVector local_topology;

  // The local (i.e. relative to the parent) geometric transformation matrix of each
  // TransformHandle. Handles with no entry indicate an identity matrix.
  std::pmr::unordered_map<TransformHandle, glm::mat3> local_matrices;

  // The local (i.e. relative to the parent) opacity values of each TransformHandles. Handles
  // with no entry indicate an opacity value of 1.0.
  std::pmr::unordered_map<TransformHandle, float> local_opacity_values;

  // Map of the regions of images used to texture renderables. These are set per-image.
  std::pmr::unordered_map<TransformHandle, ImageSampleRegion> local_image_sample_regions;

  // Map of the regions of transforms that clip child content.
  std::pmr::unordered_map<TransformHandle, TransformClipRegion> local_clip_regions;

  // The images associated with each TransformHandle.
  std::pmr::unordered_map<TransformHandle, allocation::ImageMetadata> images;

  // Map of local hit regions.
  std::pmr::unordered_map<TransformHandle, std::pmr::vector<flatland::HitRegion>>
      local_hit_regions_map;

  // The ViewRef for the root (View) of this Flatland instance.
  // Can be nullptr when not attached to the scene, otherwise must be set.
  std::shared_ptr<const ViewRef> view_ref = nullptr;

  // Set from SetDebugName(). Can be empty if the client does not calls SetDebugName().
  std::pmr::string debug_name;

  // The time the UberStruct was created.
  zx::time_monotonic creation_time;

  // Test-only helper which abstracts over legacy image content, and Flatland2 layer content.
  bool HasLayerContentForTest(TransformHandle handle) const { return images.contains(handle); }
};

}  // namespace flatland

namespace std {
ostream& operator<<(ostream& out, const flatland::UberStruct& us);
}  // namespace std

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_UBER_STRUCT_H_
