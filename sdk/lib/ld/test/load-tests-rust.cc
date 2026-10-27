// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "load-tests.h"

namespace ld::testing {
namespace {

TYPED_TEST(LdLoadTests, RustBasic) {
  constexpr int64_t kReturnValue = 23;

  ASSERT_NO_FATAL_FAILURE(this->Init());

  ASSERT_NO_FATAL_FAILURE(this->Needed({"librust-dep.so"}));

  ASSERT_NO_FATAL_FAILURE(this->Load("rust-basic"));

  EXPECT_EQ(this->Run(), kReturnValue);

  this->ExpectLog("");
}

}  // namespace
}  // namespace ld::testing
