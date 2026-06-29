// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/flatland1_state.h"

namespace flatland {

allocation::ImageMetadata* Flatland1ContentState::FindImage(TransformHandle content_handle) {
  auto it = image_metadatas.find(content_handle);
  if (it == image_metadatas.end()) {
    return nullptr;
  }
  return &it->second;
}

const allocation::ImageMetadata* Flatland1ContentState::FindImage(
    TransformHandle content_handle) const {
  auto it = image_metadatas.find(content_handle);
  if (it == image_metadatas.end()) {
    return nullptr;
  }
  return &it->second;
}

}  // namespace flatland
