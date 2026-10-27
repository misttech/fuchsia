// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <iostream>

[[clang::optnone]] void do_loop(int n) {
  for (int i = 0; i < n; i++) {
    std::cout << "Hello world!" << std::endl;
  }
}

int main() {
#if defined(INFINITE_LOOP)
  while (true) {
    do_loop(0);
  }
#else
  do_loop(5);
#endif
}
