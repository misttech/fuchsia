// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/unittest/unittest.h>

extern "C" {
bool test_cbuf_constructor();
bool test_cbuf_read_write();
bool test_cbuf_read_write_race();
bool test_cbuf_init_limits();
bool test_cbuf_uninitialized();
bool test_cbuf_wrap_around();
bool test_cbuf_blocking_read();
}

namespace {

bool rust_cbuf_constructor() {
  BEGIN_TEST;
  EXPECT_TRUE(test_cbuf_constructor());
  END_TEST;
}

bool rust_cbuf_read_write() {
  BEGIN_TEST;
  EXPECT_TRUE(test_cbuf_read_write());
  END_TEST;
}

bool rust_cbuf_read_write_race() {
  BEGIN_TEST;
  EXPECT_TRUE(test_cbuf_read_write_race());
  END_TEST;
}

bool rust_cbuf_init_limits() {
  BEGIN_TEST;
  EXPECT_TRUE(test_cbuf_init_limits());
  END_TEST;
}

bool rust_cbuf_uninitialized() {
  BEGIN_TEST;
  EXPECT_TRUE(test_cbuf_uninitialized());
  END_TEST;
}

bool rust_cbuf_wrap_around() {
  BEGIN_TEST;
  EXPECT_TRUE(test_cbuf_wrap_around());
  END_TEST;
}

bool rust_cbuf_blocking_read() {
  BEGIN_TEST;
  EXPECT_TRUE(test_cbuf_blocking_read());
  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(rust_cbuf_tests)
UNITTEST("Constructor", rust_cbuf_constructor)
UNITTEST("ReadWrite", rust_cbuf_read_write)
UNITTEST("ReadWriteRace", rust_cbuf_read_write_race)
UNITTEST("InitLimits", rust_cbuf_init_limits)
UNITTEST("Uninitialized", rust_cbuf_uninitialized)
UNITTEST("WrapAround", rust_cbuf_wrap_around)
UNITTEST("BlockingRead", rust_cbuf_blocking_read)
UNITTEST_END_TESTCASE(rust_cbuf_tests, "rust_cbuf", "Rust cbuf tests")
