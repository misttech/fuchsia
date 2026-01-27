// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_POWER_STATE_RECORDER_CPP_COMMON_INTERNAL_H_
#define LIB_POWER_STATE_RECORDER_CPP_COMMON_INTERNAL_H_

#include <lib/zx/clock.h>
#include <zircon/compiler.h>

#include <algorithm>
#include <mutex>
#include <string>
#include <vector>

namespace power_observability::internal {

zx_ticks_t boot_time_to_ticks(zx::time_boot timestamp);

}  // namespace power_observability::internal

#endif  // LIB_POWER_STATE_RECORDER_CPP_COMMON_INTERNAL_H_
