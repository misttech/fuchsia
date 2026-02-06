// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_DSO_CPP_ASYNC_H_
#define SRC_LIB_DSO_CPP_ASYNC_H_

#include <lib/fdf/dispatcher.h>

// Defined by client
int dso_main_async(int argc, const char** argv, const char** envp, fdf_dispatcher_t* dispatcher);

#endif  // SRC_LIB_DSO_CPP_ASYNC_H_
