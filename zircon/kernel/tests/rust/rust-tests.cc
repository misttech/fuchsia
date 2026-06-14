// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/unittest/unittest.h>
#include <stdint.h>

#include <arch/ops.h>

extern "C" {  // Defined in Rust (src/lib.rs).

int32_t add_one_in_rust(int32_t);

extern const int32_t kConstVarDefinedInRust;

extern int32_t gVarDefinedInRust;

extern const int32_t kConstVarExportedToRust = 23;
int32_t get_const_var_exported_to_rust();

int32_t gVarExportedToRust;
int32_t fetch_add_var_exported_to_rust(int32_t);
bool test_rust_interrupt_ops();
uint32_t test_rust_curr_cpu_num();
uint32_t test_rust_max_num_cpus();

}  // extern "C"

namespace {

bool add_one_test() {
  BEGIN_TEST;
  EXPECT_EQ(add_one_in_rust(42), 43);
  END_TEST;
}

bool defined_const_test() {
  BEGIN_TEST;
  EXPECT_EQ(kConstVarDefinedInRust, 17);
  END_TEST;
}

bool exported_const_test() {
  BEGIN_TEST;
  EXPECT_EQ(get_const_var_exported_to_rust(), 23);
  END_TEST;
}

bool defined_var_test() {
  BEGIN_TEST;
  EXPECT_EQ(gVarDefinedInRust, 42);
  END_TEST;
}

bool exported_var_test() {
  BEGIN_TEST;
  gVarExportedToRust = 17;
  EXPECT_EQ(fetch_add_var_exported_to_rust(23), 17);
  EXPECT_EQ(gVarExportedToRust, 40);
  END_TEST;
}

bool rust_interrupt_ops_test() {
  BEGIN_TEST;
  EXPECT_TRUE(test_rust_interrupt_ops());
  END_TEST;
}

bool rust_curr_cpu_num_test() {
  BEGIN_TEST;
  EXPECT_EQ(test_rust_curr_cpu_num(), arch_curr_cpu_num());
  END_TEST;
}

bool rust_max_num_cpus_test() {
  BEGIN_TEST;
  EXPECT_EQ(test_rust_max_num_cpus(), arch_max_num_cpus());
  END_TEST;
}

UNITTEST_START_TESTCASE(rust_tests)
UNITTEST("test a trivial Rust function called from C++", add_one_test)
UNITTEST("test a Rust-defined global variable read from C++", defined_const_test)
UNITTEST("test a C++-defined global variable read from Rust", exported_const_test)
UNITTEST("test a Rust-defined global variable written from C++", defined_var_test)
UNITTEST("test a C++-defined global variable written from Rust", exported_var_test)
UNITTEST("test Rust interrupt enable/disable/disabled ops", rust_interrupt_ops_test)
UNITTEST("test Rust current CPU number", rust_curr_cpu_num_test)
UNITTEST("test Rust max num CPUs", rust_max_num_cpus_test)
UNITTEST_END_TESTCASE(rust_tests, "rust", "Tests for Rust")

}  // namespace
