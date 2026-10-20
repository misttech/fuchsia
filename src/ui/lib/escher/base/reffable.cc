// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/base/reffable.h"

#include <lib/syslog/cpp/macros.h>

namespace escher {

Reffable::~Reffable() { FX_DCHECK(ref_count_.load(std::memory_order_relaxed) == 0u); }

#ifndef NDEBUG
void Reffable::Adopt() {
  FX_DCHECK(adoption_required_);
  FX_DCHECK(ref_count_.load(std::memory_order_relaxed) == 1u);
  adoption_required_ = false;
}
#endif

}  // namespace escher
