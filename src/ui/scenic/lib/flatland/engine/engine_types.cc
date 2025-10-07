// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/engine/engine_types.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <fidl/fuchsia.math/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>

namespace {

using fuchsia::ui::composition::Orientation;
using fuchsia_ui_composition::ImageFlip;

}  // namespace
namespace flatland {

DisplaySrcDstFrames DisplaySrcDstFrames::New(ImageRect rectangle, allocation::ImageMetadata image) {
  types::Rectangle image_source({
      .x = rectangle.texel_uvs[0].x,
      .y = rectangle.texel_uvs[0].y,
      .width = rectangle.texel_uvs[2].x - rectangle.texel_uvs[0].x,
      .height = rectangle.texel_uvs[2].y - rectangle.texel_uvs[0].y,
  });

  types::Rectangle display_destination({
      .x = static_cast<int32_t>(rectangle.origin.x),
      .y = static_cast<int32_t>(rectangle.origin.y),
      .width = static_cast<int32_t>(rectangle.extent.x),
      .height = static_cast<int32_t>(rectangle.extent.y),
  });
  return {.src = image_source, .dst = display_destination};
}

}  // namespace flatland
