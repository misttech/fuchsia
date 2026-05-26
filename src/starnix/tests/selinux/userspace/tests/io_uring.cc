// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/syscall.h>
#include <unistd.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/io_uring.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

extern std::string DoPrePolicyLoadWork() { return "io_uring_policy"; }

namespace {

int io_uring_setup(uint32_t entries, io_uring_params* params) {
  return static_cast<int>(syscall(__NR_io_uring_setup, entries, params));
}

TEST(IoUringTest, IoUringSqpollDenied) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:io_uring_test_no_sqpoll_t:s0", [] {
    struct io_uring_params params = {};
    params.flags = IORING_SETUP_SQPOLL;
    fbl::unique_fd ring_fd(io_uring_setup(2, &params));
    EXPECT_FALSE(ring_fd.is_valid());
    EXPECT_EQ(errno, EACCES);
  }));
}

TEST(IoUringTest, IoUringSqpollAllowed) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:io_uring_test_yes_sqpoll_t:s0", [] {
    struct io_uring_params params = {};
    params.flags = IORING_SETUP_SQPOLL;
    // TODO(https://fxbug.dev/515346567): The "io_uring { setup }" permission
    // will be needed once this test starts running on GKI based on Linux 6.18.
    fbl::unique_fd ring_fd(io_uring_setup(2, &params));
    EXPECT_TRUE(ring_fd.is_valid()) << strerror(errno);
  }));
}

}  // namespace
