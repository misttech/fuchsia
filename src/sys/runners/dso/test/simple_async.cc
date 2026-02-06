// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <stdint.h>
#include <zircon/compiler.h>

#include "src/lib/dso/cpp/async.h"

namespace {
// This is global storage, not thread local so all instances of this component in the DSO runner
// share it.
uint32_t run_counter = 0;
}  // namespace

__EXPORT
extern "C" uint32_t simple_async_read_run_counter() { return run_counter; }

int dso_main_async(int argc, const char** argv, const char** envp, fdf_dispatcher_t* dispatcher) {
  ++run_counter;
  // TODO(https://fxbug.dev/403545512): Once the lifecycle channel is passed to main immediately
  // close it now to exit the component.
  return 0;
}
