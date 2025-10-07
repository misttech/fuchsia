// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_DISPLAY_LAYER_EQUIVALENCE_H_
#define SRC_UI_SCENIC_LIB_DISPLAY_LAYER_EQUIVALENCE_H_

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <fidl/fuchsia.math/cpp/fidl.h>

#include <variant>

#include "src/ui/scenic/lib/display/fidl_typedefs.h"
#include "src/ui/scenic/lib/display/typedefs.h"
#include "src/ui/scenic/lib/types/util/hash_combine.h"

namespace display::internal {

// Variant sub-type of `LayerEquivalence`.
//
// Represents the subset of an Image Layer's configuration that defines its equivalence class for
// the `fuchsia.hardware.display.Coordinator/CheckConfig()` method.  This includes properties like
// dimensions, format, transformation, and alpha, but excludes fields like the specific `ImageId`,
// which don't affect hardware compatibility for a given frame.
//
// TODO(https://fxbug.dev/446183922): it seems like the image's pixel format must be a meaningful
// datum to take into account, but I can't find any documentation/code in the display stack to
// support this theory.
struct ImageLayerEquivalence {
  Rectangle display_destination = Rectangle({.x = 0, .y = 0, .width = 0, .height = 0});
  Rectangle image_source = Rectangle({.x = 0, .y = 0, .width = 0, .height = 0});
  RotateFlip image_source_transformation = RotateFlip::kIdentity();

  // Together, `image_dimensions` and `image_tiling_type` are equivalent to a
  // `fuchsia.hardware.display.types/ImageMetadata`.
  Extent2 image_dimensions = Extent2({.width = 0, .height = 0});
  uint32_t image_tiling_type = fuchsia_hardware_display_types::wire::kImageTilingTypeLinear;

  BlendMode blend_mode = BlendMode::kReplace();

  // TODO(https://fxbug.dev/446042966): different alpha values change the hash/equality of the spec.
  // This means that changing a layer from alpha=0.45 to alpha=0.44 will require an additional
  // `CheckConfig()`, and also has has the potential to blow up the size of the config cache.
  // Consider moving `float alpha_value` into `display::internal::Layer`, and replacing it here with
  // an enum type.
  float alpha_value = 1.f;

  constexpr bool operator==(const ImageLayerEquivalence& other) const {
    return display_destination == other.display_destination && image_source == other.image_source &&
           image_source_transformation == other.image_source_transformation &&
           image_dimensions == other.image_dimensions &&
           image_tiling_type == other.image_tiling_type && blend_mode == other.blend_mode &&
           alpha_value == other.alpha_value;
  }
};

// Variant sub-type of `LayerEquivalence`.
//
// Represents the subset of a Color Layer's configuration that defines its equivalence class for the
// `fuchsia.hardware.display.Coordinator/CheckConfig()` method.
struct ColorLayerEquivalence {
  WireColor color = {
      .format = fuchsia_images2::PixelFormat::kInvalid,
      .bytes = {},
  };
  Rectangle display_destination = Rectangle({.x = 0, .y = 0, .width = 0, .height = 0});

  constexpr bool operator==(const ColorLayerEquivalence& other) const {
    return color.format == other.color.format && color.bytes == other.color.bytes &&
           display_destination == other.display_destination;
  }
};

// Variant sub-type of `LayerEquivalence`.  This is the default value for a new `LayerEquivalence`.
struct UninitializedLayerEquivalence {
  constexpr bool operator==(const UninitializedLayerEquivalence& other) const { return true; }
};

// Represents the `CheckConfig()`-relevant properties of a single layer, defining its equivalence
// class.  This is a variant type which initially holds an `UninitializedLayerEquivalence`, and
// which can subsequently be set to either an `ImageLayerEquivalence` or a `ColorLayerEquivalence`.
struct LayerEquivalence {
  std::variant<UninitializedLayerEquivalence, ImageLayerEquivalence, ColorLayerEquivalence> config;

  constexpr bool operator==(const LayerEquivalence& other) const { return config == other.config; }

  LayerEquivalence& operator=(const ImageLayerEquivalence& image_ls) {
    config = image_ls;
    return *this;
  }

  LayerEquivalence& operator=(const ColorLayerEquivalence& color_ls) {
    config = color_ls;
    return *this;
  }
};

}  // namespace display::internal

namespace std {

template <>
struct hash<display::internal::ImageLayerEquivalence> {
  std::size_t operator()(const display::internal::ImageLayerEquivalence& image_ls) {
    // Random seed (`openssl rand -hex 8`) avoids collisions with types with the same memory layout.
    std::size_t seed = 0x4240a6155ac4cdfa;
    types::hash_combine(seed, image_ls.display_destination);
    types::hash_combine(seed, image_ls.image_source);
    types::hash_combine(seed, image_ls.image_source_transformation);
    types::hash_combine(seed, image_ls.image_dimensions);
    types::hash_combine(seed, image_ls.image_tiling_type);
    types::hash_combine(seed, image_ls.blend_mode);
    types::hash_combine(seed, image_ls.alpha_value);
    return seed;
  }
};

template <>
struct hash<display::internal::ColorLayerEquivalence> {
  std::size_t operator()(const display::internal::ColorLayerEquivalence& color_ls) {
    // Random seed (`openssl rand -hex 8`) avoids collisions with types with the same memory layout.
    std::size_t seed = 0xed5f7145816d6d96;
    types::hash_combine(seed, static_cast<uint32_t>(color_ls.color.format));
    {
      // There's no std::hash for std:array.
      const uint64_t bytes = std::bit_cast<uint64_t>(color_ls.color.bytes);
      types::hash_combine(seed, bytes);
    }
    types::hash_combine(seed, color_ls.display_destination);

    return seed;
  }
};

template <>
struct hash<display::internal::UninitializedLayerEquivalence> {
  std::size_t operator()(const display::internal::UninitializedLayerEquivalence& uninitialized_ls) {
    // Random seed (`openssl rand -hex 8`) avoids collisions with types with the same memory layout.
    return 0x493a4982c7cb5c28;
  }
};

template <>
struct hash<display::internal::LayerEquivalence> {
  std::size_t operator()(const display::internal::LayerEquivalence& ls) {
    // Random seed (`openssl rand -hex 8`) avoids collisions with types with the same memory layout.
    std::size_t seed = 0x06cc66fb9d5ca0c5;
    if (const display::internal::ImageLayerEquivalence* image_ls =
            std::get_if<display::internal::ImageLayerEquivalence>(&ls.config)) {
      types::hash_combine(seed, *image_ls);
      return seed;
    }
    if (const display::internal::ColorLayerEquivalence* color_ls =
            std::get_if<display::internal::ColorLayerEquivalence>(&ls.config)) {
      types::hash_combine(seed, *color_ls);
      return seed;
    }
    if (const display::internal::UninitializedLayerEquivalence* uninitialized_ls =
            std::get_if<display::internal::UninitializedLayerEquivalence>(&ls.config)) {
      types::hash_combine(seed, *uninitialized_ls);
      return seed;
    }
    __UNREACHABLE;
    return seed;
  }
};

}  // namespace std

#endif  // SRC_UI_SCENIC_LIB_DISPLAY_LAYER_EQUIVALENCE_H_
