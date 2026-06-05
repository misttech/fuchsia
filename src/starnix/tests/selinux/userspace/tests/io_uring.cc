// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/io_uring.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/io_uring_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

extern std::string DoPrePolicyLoadWork() { return "io_uring_policy"; }

namespace {

#ifndef IORING_OP_URING_CMD
#define IORING_OP_URING_CMD 46
#endif

using io_uring_helper::io_uring_enter;
using io_uring_helper::io_uring_setup;

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

struct IoUringCmdTestParam {
  std::string security_context;
  bool allowed;
};

class IoUringCmdTest : public testing::TestWithParam<IoUringCmdTestParam> {};

TEST_P(IoUringCmdTest, Cmd) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  const auto param = GetParam();

  // Sockets file descriptors are one of the few types of file
  // descriptors that supports IORING_OP_URING_CMD.
  auto fd = fbl::unique_fd(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_TRUE(fd) << strerror(errno);

  ASSERT_TRUE(RunSubprocessAs(param.security_context, [fd = std::move(fd), param] {
    auto io_uring_res = io_uring_helper::IoUring::Create(2);
    ASSERT_TRUE(io_uring_res.is_ok()) << strerror(io_uring_res.error_value());
    auto ring = std::move(io_uring_res.value());
    const auto& params = ring->params();

    uint32_t tail = ring->sq_tail_ptr()->load(std::memory_order_acquire);
    uint32_t write_index = tail & (params.sq_entries - 1);
    ring->sqes()[write_index].opcode = IORING_OP_URING_CMD;
    ring->sqes()[write_index].fd = fd.get();
    ring->sqes()[write_index].user_data = 123;

    ring->sq_array()[write_index] = write_index;
    ring->sq_tail_ptr()->store(tail + 1, std::memory_order_release);

    ASSERT_EQ(io_uring_enter(ring->fd(), 1, 1, IORING_ENTER_GETEVENTS, nullptr), 1);

    uint32_t head = ring->cq_head_ptr()->load(std::memory_order_acquire);
    ASSERT_EQ(ring->cq_tail_ptr()->load(std::memory_order_acquire) - head, 1U);

    io_uring_cqe* cqe = &ring->cqes()[head & (params.cq_entries - 1)];
    EXPECT_EQ(cqe->user_data, 123U);
    if (param.allowed) {
      EXPECT_EQ(cqe->res, 0);
    } else {
      EXPECT_EQ(cqe->res, -EACCES);
    }
  }));
}

INSTANTIATE_TEST_SUITE_P(IoUringTest, IoUringCmdTest,
                         ::testing::Values(
                             IoUringCmdTestParam{
                                 .security_context = "test_u:test_r:io_uring_test_no_cmd_t:s0",
                                 .allowed = false,
                             },
                             IoUringCmdTestParam{
                                 .security_context = "test_u:test_r:io_uring_test_yes_cmd_t:s0",
                                 .allowed = true,
                             }),
                         [](const testing::TestParamInfo<IoUringCmdTestParam>& info) {
                           return info.param.allowed ? "Allowed" : "Denied";
                         });

}  // namespace
