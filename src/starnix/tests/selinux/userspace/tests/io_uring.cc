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

#ifndef IORING_REGISTER_PERSONALITY
#define IORING_REGISTER_PERSONALITY 9
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

struct IoUringTestParam {
  std::string security_context;
  bool allowed;
};

class IoUringCmdTest : public testing::TestWithParam<IoUringTestParam> {};

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
                             IoUringTestParam{
                                 .security_context = "test_u:test_r:io_uring_test_no_cmd_t:s0",
                                 .allowed = false,
                             },
                             IoUringTestParam{
                                 .security_context = "test_u:test_r:io_uring_test_yes_cmd_t:s0",
                                 .allowed = true,
                             }),
                         [](const testing::TestParamInfo<IoUringTestParam>& info) {
                           return info.param.allowed ? "Allowed" : "Denied";
                         });

class IoUringOverrideCredsTest : public testing::TestWithParam<IoUringTestParam> {};

// The sysroot that is used to build tests does not have an up-to-date definition of the
// io_uring_sqe struct. Specifically, it is missing the personality field, so we use this local
// definition derived from the following man page:
// https://man7.org/linux/man-pages/man7/io_uring.7.html
struct local_io_uring_sqe {
  __u8 opcode;
  __u8 flags;
  __u16 ioprio;
  __s32 fd;
  union {
    __u64 off;
    __u64 addr2;
    struct {
      __u32 cmd_op;
      __u32 __pad1;
    };
  };
  union {
    __u64 addr;
    __u64 splice_off_in;
    struct {
      __u32 level;
      __u32 optname;
    };
  };
  __u32 len;
  union {
    __kernel_rwf_t rw_flags;
    __u32 fsync_flags;
    __u16 poll_events;
    __u32 poll32_events;
    __u32 sync_range_flags;
    __u32 msg_flags;
    __u32 timeout_flags;
    __u32 accept_flags;
    __u32 cancel_flags;
    __u32 open_flags;
    __u32 statx_flags;
    __u32 fadvise_advice;
    __u32 splice_flags;
    __u32 rename_flags;
    __u32 unlink_flags;
    __u32 hardlink_flags;
    __u32 xattr_flags;
    __u32 msg_ring_flags;
    __u32 uring_cmd_flags;
    __u32 waitid_flags;
    __u32 futex_flags;
    __u32 install_fd_flags;
    __u32 nop_flags;
  };
  __u64 user_data;
  union {
    __u16 buf_index;
    __u16 buf_group;
  } __attribute__((packed));
  __u16 personality;
  union {
    __s32 splice_fd_in;
    __u32 file_index;
    __u32 optlen;
    struct {
      __u16 addr_len;
      __u16 __pad3[1];
    };
  };
  union {
    struct {
      __u64 addr3;
      __u64 __pad2[1];
    };
    __u64 optval;
    __u8 cmd[0];
  };
};

// Tests that submitting an io_uring request with a registered credential personality
// requires the `override_creds` permission on the `io_uring` class.
//
// Specifically:
// 1. Registers the current thread's credentials (personality) with the io_uring.
// 2. Submits a NOP request using that registered personality.
// 3. Verifies that the request succeeds if the security context has the `override_creds`
//    permission, or fails with EACCES if it does not.
TEST_P(IoUringOverrideCredsTest, OverrideCreds) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  const auto param = GetParam();

  ASSERT_TRUE(RunSubprocessAs(param.security_context, [&] {
    // Create the ring
    auto io_uring_res = io_uring_helper::IoUring::Create(2);
    ASSERT_TRUE(io_uring_res.is_ok()) << strerror(io_uring_res.error_value());
    auto ring = std::move(io_uring_res.value());
    const auto& params = ring->params();

    // Register the current thread's credentials.
    const long personality_id =
        syscall(__NR_io_uring_register, ring->fd(), IORING_REGISTER_PERSONALITY, nullptr, 0);
    ASSERT_GT(personality_id, 0) << strerror(errno);

    // Submit a NOP request with the registered personality.
    uint32_t tail = ring->sq_tail_ptr()->load(std::memory_order_acquire);
    uint32_t index = tail & (params.sq_entries - 1);
    auto* sqe = reinterpret_cast<local_io_uring_sqe*>(&ring->sqes()[index]);
    sqe->opcode = IORING_OP_NOP;
    sqe->personality = static_cast<uint16_t>(personality_id);
    sqe->user_data = 123;

    ring->sq_array()[index] = index;
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

INSTANTIATE_TEST_SUITE_P(
    IoUringTest, IoUringOverrideCredsTest,
    ::testing::Values(
        IoUringTestParam{
            .security_context = "test_u:test_r:io_uring_test_no_override_t:s0",
            .allowed = false,
        },
        IoUringTestParam{
            .security_context = "test_u:test_r:io_uring_test_yes_override_t:s0",
            .allowed = true,
        }),
    [](const testing::TestParamInfo<IoUringTestParam>& info) {
      return info.param.allowed ? "Allowed" : "Denied";
    });

}  // namespace
