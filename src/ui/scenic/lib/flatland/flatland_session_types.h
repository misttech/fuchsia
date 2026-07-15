// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_SESSION_TYPES_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_SESSION_TYPES_H_

#include <fidl/fuchsia.ui.composition/cpp/fidl.h>

#include <vector>

#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/uber_struct.h"
#include "src/ui/scenic/lib/types/id_type.h"

namespace flatland {

namespace internal {
using ContentIdTraits =
    types::DefaultIdTypeTraitsForNaturalFidl<uint64_t, fuchsia_ui_composition::ContentId>;
using TransformIdTraits =
    types::DefaultIdTypeTraitsForNaturalFidl<uint64_t, fuchsia_ui_composition::TransformId>;
}  // namespace internal

using ContentId = types::IdType<::flatland::internal::ContentIdTraits>;
using TransformId = types::IdType<::flatland::internal::TransformIdTraits>;

constexpr ContentId kInvalidContentId = ContentId(0);
constexpr TransformId kInvalidTransformId = TransformId(0);

// TODO(https://fxbug.dev/523371761): document.
struct LayerObject : public UberStructLayer {
  int32_t ref_count = 0;
};

struct LayerStackData {
  std::vector<LayerHandle> layers;
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_SESSION_TYPES_H_
