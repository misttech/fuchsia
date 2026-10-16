// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <iostream>

#include "asm_func.h"
#include "c_lib.h"
#include "cc_lib.h"

#if !defined(EXAMPLE_MACRO) || EXAMPLE_MACRO != 1
#error "EXAMPLE_MACRO is not properly defined!"
#endif

int main() {
  int a = c_example_add(2, 3);
  int b = cc_example::Multiply(a, 4);
  int c = asm_example_identity(b);
  std::cout << "Result: " << c << std::endl;
  return 0;
}
