// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/testonly-syscalls.h>

#include <zxtest/zxtest.h>

namespace {

TEST(MBMQTest, MBOCreate) {
  zx_handle_t handle = 0;
  ASSERT_EQ(zx_mbo_create(0, &handle), ZX_ERR_NOT_SUPPORTED);
  ASSERT_EQ(handle, 0);
}

}  // namespace
