// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef BUILD_BAZEL_RULES_TESTS_ZX_LIBRARY_INCLUDE_ZX_LIBRARY_TEST_LIB_H_
#define BUILD_BAZEL_RULES_TESTS_ZX_LIBRARY_INCLUDE_ZX_LIBRARY_TEST_LIB_H_

#include <lib/fit/function.h>

int zx_library_test_call(fit::function<int()> f);

#endif  // BUILD_BAZEL_RULES_TESTS_ZX_LIBRARY_INCLUDE_ZX_LIBRARY_TEST_LIB_H_
