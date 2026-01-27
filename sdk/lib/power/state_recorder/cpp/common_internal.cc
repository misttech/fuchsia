// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/power/state_recorder/cpp/common_internal.h"

namespace power_observability::internal {

zx_ticks_t boot_time_to_ticks(zx::time_boot timestamp) {
  // Boot ticks and boot nanos are related by a simple ratio; ticks==0 ==> nanos==0.
  __int128_t ticks =
      static_cast<__uint128_t>(timestamp.get()) * zx_ticks_per_second() / zx::sec(1).to_nsecs();

  return static_cast<zx_ticks_t>(ticks);
}

}  // namespace power_observability::internal
