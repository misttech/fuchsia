// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/dso/cpp/async.h"

#include <zircon/compiler.h>
#include <zircon/types.h>

extern "C" struct dso_async_input {
  uint32_t handle_count;
  zx_handle_t* handle;
  uint32_t* handle_info;
  uint32_t name_count;
  const char** names;
  int argc;
  const char** argv;
  const char** envp;
  fdf_dispatcher_t* dispatcher;
};

typedef struct dso_async_input dso_async_input_t;

__EXPORT
extern "C" int _dso_start_async(dso_async_input_t input) {
  // TODO(https://fxbug.dev/403545512): Fill in the implementation of this routine to extract
  // relevant handles and pass them to dso_main_async.
  const int code = dso_main_async(input.argc, input.argv, input.envp, input.dispatcher);
  // Don't call libc finalize, component is still running.
  return code;
}
