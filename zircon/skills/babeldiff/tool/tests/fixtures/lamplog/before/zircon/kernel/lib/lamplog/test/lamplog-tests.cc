// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// A synthetic lamp log test suite for babeldiff's tests, trimmed from the
// kernel's crashlog tests. It is not real kernel code.

#include <lib/lamplog.h>
#include <lib/unittest/unittest.h>

#include <ktl/array.h>
#include <ktl/string_view.h>
#include <ktl/unique_ptr.h>

namespace {

constexpr size_t kLargeBufferSize = 8 * 1024;

bool NoGlowTest() {
  BEGIN_TEST;

  fbl::AllocChecker ac;
  auto buffer = ktl::make_unique<char[]>(&ac, kLargeBufferSize);
  ASSERT_TRUE(ac.check());

  // NoGlow should currently produce no log payload at all.
  size_t len = lamplog_to_string({buffer.get(), kLargeBufferSize}, LampOffReason::NoGlow);
  EXPECT_EQ(0u, len);

  END_TEST;
}

bool FlickerTest() {
  BEGIN_TEST;

  fbl::AllocChecker ac;
  auto buffer = ktl::make_unique<char[]>(&ac, kLargeBufferSize);
  ASSERT_TRUE(ac.check());

  // Flicker and Burnout reasons should produce all of the sections.
  constexpr ktl::array kReasons = {LampOffReason::Flicker, LampOffReason::Burnout};

  for (auto reason : kReasons) {
    memset(buffer.get(), 0, kLargeBufferSize);
    size_t len = lamplog_to_string({buffer.get(), kLargeBufferSize}, reason);
    ASSERT_LE(len, kLargeBufferSize);
    ASSERT_GT(len, 0u);

    ktl::string_view text{buffer.get(), len};

    // Wick info.
    EXPECT_TRUE(text.find("WICK"sv) != ktl::string_view::npos);

    // Glow log.
    EXPECT_TRUE(text.find("--- BEGIN GLOW DUMP ---"sv) != ktl::string_view::npos);
    EXPECT_TRUE(text.find("--- END GLOW DUMP ---"sv) != ktl::string_view::npos);
  }

  END_TEST;
}

bool DimTest() {
  BEGIN_TEST;

  fbl::AllocChecker ac;
  auto buffer = ktl::make_unique<char[]>(&ac, kLargeBufferSize);
  ASSERT_TRUE(ac.check());

  // Dim should produce just the banner.
  size_t len = lamplog_to_string({buffer.get(), kLargeBufferSize}, LampOffReason::Dim);
  EXPECT_GT(len, 0u);

  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(lamplog_tests)
UNITTEST("NoGlow", NoGlowTest)
UNITTEST("Flicker", FlickerTest)
UNITTEST("Dim", DimTest)
UNITTEST_END_TESTCASE(lamplog_tests, "lamplog", "lamplog tests")
