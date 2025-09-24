// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_DISPLAY_DISPLAY_SPEC_H_
#define SRC_UI_SCENIC_LIB_DISPLAY_DISPLAY_SPEC_H_

#include "src/ui/scenic/lib/display/layer_spec.h"
#include "src/ui/scenic/lib/types/display_mode.h"

namespace display::internal {

// Represents the subset of the Display's configuration relevant for
// `fuchsia.hardware.display.Coordinator/CheckConfig()`. This aggregates `LayerSpec`s for active
// layers and display-wide properties like mode and color conversion settings.
//
// This is used to key the cache for `CheckConfig()` results. The order of layers in the `layers`
// vector matters, as it corresponds to the order provided to the display hardware. However, the
// specific `LayerId` values used in the application are not part of the spec, only the properties
// of the layers themselves in their specified order.
struct DisplaySpec {
  std::vector<LayerSpec> layers;
  types::DisplayMode display_mode;
  // TODO(https://fxbug.dev/446042966): small changes to these might cause us to thrash the
  // `CheckConfig()` cache in `CoordinatorProxy`.  See bug for possible mitigations.
  std::array<float, 3> color_conversion_preoffsets = {};
  std::array<float, 9> color_conversion_coefficients = {};
  std::array<float, 3> color_conversion_postoffsets = {};

  constexpr bool operator==(const DisplaySpec& other) const {
    return layers == other.layers && display_mode == other.display_mode &&
           color_conversion_preoffsets == other.color_conversion_preoffsets &&
           color_conversion_coefficients == other.color_conversion_coefficients &&
           color_conversion_postoffsets == other.color_conversion_postoffsets;
  };
};

}  // namespace display::internal

namespace std {

template <>
struct hash<display::internal::DisplaySpec> {
  std::size_t operator()(const display::internal::DisplaySpec& spec) const {
    // Random seed (`openssl rand -hex 8`) avoids collisions with types with the same memory layout.
    std::size_t seed = 0x5804602f4cac9f58;
    for (auto& layer : spec.layers) {
      types::hash_combine(seed, layer);
    }
    types::hash_combine(seed, spec.display_mode);

    // std::array doesn't have std::hash specialization.
    // We use "Golden Ratio" hashing, as popularized by Knuth.
    std::hash<float> hasher;
    constexpr std::size_t kGoldenRatio = 0x9e3779b9;
    for (const auto& val : spec.color_conversion_preoffsets) {
      seed ^= hasher(val) + kGoldenRatio + (seed << 6) + (seed >> 2);
    }
    for (const auto& val : spec.color_conversion_coefficients) {
      seed ^= hasher(val) + kGoldenRatio + (seed << 6) + (seed >> 2);
    }
    for (const auto& val : spec.color_conversion_postoffsets) {
      seed ^= hasher(val) + kGoldenRatio + (seed << 6) + (seed >> 2);
    }

    return seed;
  }
};

}  // namespace std

#endif  // SRC_UI_SCENIC_LIB_DISPLAY_DISPLAY_SPEC_H_
