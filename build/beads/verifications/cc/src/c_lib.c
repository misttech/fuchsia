// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "c_lib.h"

#if !defined(EXAMPLE_MACRO) || EXAMPLE_MACRO != 1
#error "EXAMPLE_MACRO is not properly defined!"
#endif

int c_example_add(int a, int b) { return a + b; }
