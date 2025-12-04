// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "stack_sampler.h"

#include <lib/syslog/cpp/macros.h>
#include <zircon/errors.h>

namespace profiler {

zx::result<> StackSampler::Start(size_t buffer_size_mb) {
  // TODO(https://fxbug.dev/447626904): Implement stack sampling using SP/DWARF.
  FX_LOGS(ERROR) << "Stack sampling (DWARF) is not yet implemented.";
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

zx::result<> StackSampler::Stop() {
  return zx::ok();
}

}  // namespace profiler
