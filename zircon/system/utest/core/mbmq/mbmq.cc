// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/testonly-syscalls.h>

#include <zxtest/zxtest.h>

namespace {

zx_status_t mbo_create(uint32_t options, zx::handle* out) {
  return zx_mbo_create(options, out->reset_and_get_address());
}

TEST(MbmqTest, MboWriteAndRead) {
  zx::handle mbo;
  ASSERT_OK(mbo_create(0, &mbo));

  static const char kMessage[] = "example message";
  ASSERT_OK(zx_mbo_write(mbo.get(), 0, kMessage, sizeof(kMessage), nullptr, 0));

  char buffer[100] = {};
  uint32_t actual_bytes = 999;
  uint32_t actual_handles = 999;
  ASSERT_OK(zx_mbo_read(mbo.get(), 0, buffer, nullptr, sizeof(buffer), 0, &actual_bytes,
                        &actual_handles));
  ASSERT_EQ(actual_bytes, sizeof(kMessage));
  ASSERT_EQ(actual_handles, 0);
  ASSERT_EQ(memcmp(buffer, kMessage, actual_bytes), 0);

  // TODO: test read and write of handles
  // TODO: test error case where buffer is too small
  // TODO: test reading twice
  // TODO: test writing twice
}

}  // namespace
