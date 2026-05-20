// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/unittest/unittest.h>
#include <stdint.h>

extern "C" {  // Defined in Rust (src/lib.rs).

int32_t add_one_in_rust(int32_t);

}  // extern "C"

namespace {

bool add_one_test() {
  BEGIN_TEST;
  EXPECT_EQ(add_one_in_rust(42), 43);
  END_TEST;
}

UNITTEST_START_TESTCASE(rust_tests)
UNITTEST("test a trivial Rust function called from C++", add_one_test)
UNITTEST_END_TESTCASE(rust_tests, "rust", "Tests for Rust")

}  // namespace
