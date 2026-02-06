// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/dso/cpp/async.h"

int dso_main_async(int argc, const char** argv, const char** envp, fdf_dispatcher_t* dispatcher) {
  // Returning a non-zero code here will cause the runner to immediately terminate the component,
  // there is no need to shutdown the dispatcher here.
  return 128;
}
