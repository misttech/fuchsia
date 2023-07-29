// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/testonly-syscalls.h>

#include <zxtest/zxtest.h>

namespace {

TEST(MBMQTest, MBOCreate) {
  zx::handle handle;
  ASSERT_OK(zx_mbo_create(0, handle.reset_and_get_address()));
  ASSERT_NE(handle.get(), 0);
}

}  // namespace
