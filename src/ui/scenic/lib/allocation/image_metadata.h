// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_ALLOCATION_IMAGE_METADATA_H_
#define SRC_UI_SCENIC_LIB_ALLOCATION_IMAGE_METADATA_H_

// Remove when C++-23 is available.
#include <lib/stdcompat/utility.h>

#include "src/ui/scenic/lib/allocation/id.h"
#include "src/ui/scenic/lib/types/blend_mode.h"

namespace allocation {

// Struct representing the data needed to extract an image from a buffer collection.
// All pixel information is stored within the Vmo of the collection so this struct
// only needs information regarding which collection and which vmo to point to, and
// the overall size of the image. Only supports fuchsia::images2::PixelFormat::B8G8R8A8
// as the image format type.
struct ImageMetadata {
  // The unique id of the buffer collection this image is backed by.
  GlobalBufferCollectionId collection_id = kInvalidId;

  // The unique ID for this particular image.
  display::ImageId identifier = display::kInvalidImageId;

  // A single buffer collection may have several vmos. This tells the importer
  // which vmo in the collection specified by |collection_id| to use as the memory
  // for this image. This value must be less than the total number of vmos of the
  // buffer collection we are constructing the image from.
  uint32_t vmo_index;

  // The dimensions of the image in pixels.
  uint32_t width = 0;
  uint32_t height = 0;

  // Linear-space RGBA values to multiply with the pixel values of the image.
  std::array<float, 4> multiply_color = {1.f, 1.f, 1.f, 1.f};

  // The blend mode to use when compositing this image.
  types::BlendMode blend_mode = types::BlendMode::kReplace();

  // The flip/reflection mode to use for this particular image.
  fuchsia_ui_composition::ImageFlip flip = fuchsia_ui_composition::ImageFlip::kNone;

  bool operator==(const ImageMetadata& other) const {
    return (collection_id == other.collection_id && vmo_index == other.vmo_index &&
            width == other.width && height == other.height && blend_mode == other.blend_mode &&
            flip == other.flip && multiply_color == other.multiply_color);
  }
};

inline std::ostream& operator<<(std::ostream& str, const ImageMetadata& m) {
  str << "size=" << (m.collection_id == kInvalidId ? 1 : m.width) << "x"
      << (m.collection_id == kInvalidId ? 1 : m.height) << "  multiply_color=("
      << m.multiply_color[0] << "," << m.multiply_color[1] << "," << m.multiply_color[2] << ","
      << m.multiply_color[3] << ")" << (m.collection_id == kInvalidId ? " (Solid Color)" : "")
      << "  blend_mode=" << m.blend_mode << " flip=" << cpp23::to_underlying(m.flip);
  return str;
}

}  // namespace allocation

#endif  // SRC_UI_SCENIC_LIB_ALLOCATION_IMAGE_METADATA_H_
