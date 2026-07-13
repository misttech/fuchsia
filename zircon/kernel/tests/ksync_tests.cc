// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/unittest/unittest.h>
#include <stdint.h>

extern "C" {

bool test_ksync_spinlock();
bool test_ksync_mutex();
bool test_ksync_event();
bool test_ksync_brwlock();

}  // extern "C"

namespace {

bool rust_spinlock_test() {
  BEGIN_TEST;
  EXPECT_TRUE(test_ksync_spinlock());
  END_TEST;
}

bool rust_mutex_test() {
  BEGIN_TEST;
  EXPECT_TRUE(test_ksync_mutex());
  END_TEST;
}

bool rust_event_test() {
  BEGIN_TEST;
  EXPECT_TRUE(test_ksync_event());
  END_TEST;
}

bool rust_brwlock_test() {
  BEGIN_TEST;
  EXPECT_TRUE(test_ksync_brwlock());
  END_TEST;
}

UNITTEST_START_TESTCASE(rust_ksync_tests)
UNITTEST("test Rust KSpinlock", rust_spinlock_test)
UNITTEST("test Rust KMutex", rust_mutex_test)
UNITTEST("test Rust KEvent", rust_event_test)
UNITTEST("test Rust BrwLockPi", rust_brwlock_test)
UNITTEST_END_TESTCASE(rust_ksync_tests, "rust_ksync", "Tests for Rust ksync bindings")

}  // namespace
