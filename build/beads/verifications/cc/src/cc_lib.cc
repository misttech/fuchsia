// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "cc_lib.h"

#if !defined(EXAMPLE_MACRO) || EXAMPLE_MACRO != 1
#error "EXAMPLE_MACRO is not properly defined!"
#endif

namespace cc_example {

int Multiply(int a, int b) { return a * b; }

}  // namespace cc_example
