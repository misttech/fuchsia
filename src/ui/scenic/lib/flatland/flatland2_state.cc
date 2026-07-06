// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/flatland2_state.h"

namespace flatland {

std::ostream& operator<<(std::ostream& out, const LayerHandle& h) {
  out << "(L:" << h.GetInstanceId() << ":" << h.GetLayerId() << ")";
  return out;
}

}  // namespace flatland
