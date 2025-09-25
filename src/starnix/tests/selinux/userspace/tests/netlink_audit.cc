// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/fit/result.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cerrno>
#include <cstring>
#include <fstream>
#include <string_view>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/audit.h>
#include <linux/netlink.h>

#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

extern std::string DoPrePolicyLoadWork() { return "netlink_audit.pp"; }

namespace {

/// The `audit_status` structure in `audit.h` does not have the last
/// field, which makes it incompatible with Starnix.
/// The current sysroot version of `audit.h` is out of date.
struct audit_status {
  uint32_t mask;
  uint32_t enabled;
  uint32_t failure;
  uint32_t pid;
  uint32_t rate_limit;
  uint32_t backlog_limit;
  uint32_t lost;
  uint32_t backlog;
  union {
    uint32_t version;
    uint32_t feature_bitmap;
  };
  uint32_t backlog_wait_time;
  /// Needed for Starnix compatibility.
  uint32_t backlog_wait_time_actual;
};

struct audit_request {
  struct nlmsghdr nlh;
  audit_status status;
};

constexpr int NETLINK_BUF_SIZE = 4096;

// Helper function to open a Netlink socket for Audit
fbl::unique_fd OpenNetlinkAuditSocket() {
  fbl::unique_fd fd(socket(AF_NETLINK, SOCK_RAW, NETLINK_AUDIT));
  return fd;
}

// Helper function to send a netlink message
fit::result<int, ssize_t> SendNetlinkMessage(int fd, const void* buf, size_t len) {
  ssize_t sent = send(fd, buf, len, 0);
  if ((size_t)sent != len) {
    return fit::error(errno);
  }
  return fit::ok(sent);
}

// Helper function to receive a netlink message
fit::result<int, ssize_t> ReceiveNetlinkMessage(int fd, void* buf, size_t len, bool wait = true) {
  ssize_t received = recv(fd, buf, len, wait ? 0 : MSG_DONTWAIT);
  if (received < 0) {
    return fit::error(errno);
  }
  return fit::ok(received);
}

// Helper to register the calling process as the audit daemon on a given socket.
fit::result<int> RegisterAsAuditDaemon(int fd, pid_t pid) {
  audit_request req;
  char buf[NETLINK_BUF_SIZE]{};

  memset(&req, 0, sizeof(req));
  req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.status));
  req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
  req.nlh.nlmsg_seq = 1;
  req.nlh.nlmsg_type = AUDIT_SET;
  req.status.pid = pid;
  req.status.mask = AUDIT_STATUS_PID;

  auto send_res = SendNetlinkMessage(fd, &req, req.nlh.nlmsg_len);
  if (send_res.is_error()) {
    return send_res.take_error();
  }

  auto recv_res = ReceiveNetlinkMessage(fd, buf, sizeof(buf));
  if (recv_res.is_error()) {
    return recv_res.take_error();
  }

  if (recv_res.value() < NLMSG_HDRLEN) {
    return fit::error(EINVAL);
  }

  auto* nlh = reinterpret_cast<struct nlmsghdr*>(buf);
  if (nlh->nlmsg_type != NLMSG_ERROR) {
    return fit::error(EBADMSG);
  }

  auto* err = reinterpret_cast<struct nlmsgerr*>(NLMSG_DATA(nlh));
  if (err->error) {
    return fit::error(-err->error);
  }
  return fit::ok();
}

// Helper to unregister the current audit daemon.
fit::result<int> UnregisterAuditDaemon(int fd) {
  audit_request req;
  char buf[NETLINK_BUF_SIZE]{};

  memset(&req, 0, sizeof(req));
  req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.status));
  req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
  req.nlh.nlmsg_seq = 1;
  req.nlh.nlmsg_type = AUDIT_SET;

  // Setting pid to 0 releases the daemon registration.
  req.status.pid = 0;
  req.status.mask = AUDIT_STATUS_PID;

  auto send_res = SendNetlinkMessage(fd, &req, req.nlh.nlmsg_len);
  if (send_res.is_error()) {
    return send_res.take_error();
  }

  struct nlmsghdr* nlh;
  do {
    auto recv_res = ReceiveNetlinkMessage(fd, buf, sizeof(buf));
    if (recv_res.is_error()) {
      return recv_res.take_error();
    }
    if (recv_res.value() < NLMSG_HDRLEN) {
      return fit::error(EINVAL);
    }

    nlh = reinterpret_cast<struct nlmsghdr*>(buf);
  } while (nlh->nlmsg_type != NLMSG_ERROR);

  auto* err = reinterpret_cast<struct nlmsgerr*>(NLMSG_DATA(nlh));
  if (err->error) {
    return fit::error(-err->error);
  }
  return fit::ok();
}

// Helper to send a user-space audit message (e.g., from an AVC denial).
fit::result<int> SendUserAuditMessage(int fd, int seq, const std::string& message_text,
                                      bool ack = true) {
  const size_t payload_len = message_text.length() + 1;
  const size_t buf_len = NLMSG_SPACE(payload_len);
  std::vector<char> buf(buf_len);
  char ack_buf[NETLINK_BUF_SIZE]{};
  memset(buf.data(), 0, buf_len);

  auto* nlh = reinterpret_cast<struct nlmsghdr*>(buf.data());
  nlh->nlmsg_len = NLMSG_LENGTH((int)payload_len);
  nlh->nlmsg_type = AUDIT_USER_AVC;
  nlh->nlmsg_flags = NLM_F_REQUEST;
  if (ack) {
    nlh->nlmsg_flags |= NLM_F_ACK;
  }
  nlh->nlmsg_seq = seq;

  char* data = static_cast<char*>(NLMSG_DATA(nlh));
  strncpy(data, message_text.c_str(), payload_len);

  auto send_res = SendNetlinkMessage(fd, nlh, nlh->nlmsg_len);
  if (send_res.is_error()) {
    return send_res.take_error();
  }
  if (!ack) {
    return fit::ok();
  }

  // Now, wait for the acknowledgment from the kernel.
  auto recv_res = ReceiveNetlinkMessage(fd, ack_buf, sizeof(ack_buf));
  if (recv_res.is_error()) {
    return recv_res.take_error();
  }

  if (recv_res.value() < NLMSG_HDRLEN) {
    return fit::error(EINVAL);
  }

  auto* ack_nlh = reinterpret_cast<struct nlmsghdr*>(ack_buf);
  if (ack_nlh->nlmsg_type != NLMSG_ERROR) {
    return fit::error(EBADMSG);
  }

  auto* err = reinterpret_cast<struct nlmsgerr*>(NLMSG_DATA(ack_nlh));
  if (err->error) {
    return fit::error(-err->error);
  }
  return fit::ok();
}

TEST(NetlinkAuditTest, RegisterAuditDaemon) {
  fbl::unique_fd fd = OpenNetlinkAuditSocket();
  ASSERT_TRUE(fd.is_valid());
  ASSERT_TRUE(RegisterAsAuditDaemon(fd.get(), getpid()).is_ok());
  EXPECT_TRUE(UnregisterAuditDaemon(fd.get()).is_ok());
}

TEST(NetlinkAuditTest, RegisterAuditDaemonWithOtherPid) {
  fbl::unique_fd fd = OpenNetlinkAuditSocket();
  ASSERT_TRUE(fd.is_valid());
  ASSERT_EQ(RegisterAsAuditDaemon(fd.get(), getpid() + 1), fit::error(EINVAL));
}

TEST(NetlinkAuditTest, RegisterAuditDaemonTwice) {
  fbl::unique_fd fd1 = OpenNetlinkAuditSocket();
  fbl::unique_fd fd2 = OpenNetlinkAuditSocket();
  ASSERT_TRUE(fd1.is_valid());
  ASSERT_TRUE(fd2.is_valid());

  ASSERT_TRUE(RegisterAsAuditDaemon(fd1.get(), getpid()).is_ok());
  ASSERT_EQ(RegisterAsAuditDaemon(fd2.get(), getpid()), fit::error(EEXIST));

  EXPECT_TRUE(UnregisterAuditDaemon(fd1.get()).is_ok());
}

TEST(NetlinkAuditTest, MessageReceivedOnlyOnDaemonSocket) {
  fbl::unique_fd fd1 = OpenNetlinkAuditSocket();
  fbl::unique_fd fd2 = OpenNetlinkAuditSocket();
  ASSERT_TRUE(fd1.is_valid());
  ASSERT_TRUE(fd2.is_valid());

  // Send a message on fd1 before registering it as the daemon.
  const std::string message = "test_message_case_1";
  ASSERT_TRUE(SendUserAuditMessage(fd1.get(), 101, message).is_ok());
  ASSERT_TRUE(RegisterAsAuditDaemon(fd1.get(), getpid()).is_ok());

  char buf[NETLINK_BUF_SIZE]{};
  EXPECT_EQ(ReceiveNetlinkMessage(fd2.get(), buf, sizeof(buf), false), fit::error(EAGAIN));

  // The message should be on fd1.
  struct nlmsghdr* nlh;
  do {
    auto recv_res = ReceiveNetlinkMessage(fd1.get(), buf, sizeof(buf));
    ASSERT_TRUE(recv_res.is_ok());
    ASSERT_GE(recv_res.value(), NLMSG_HDRLEN);
    nlh = reinterpret_cast<struct nlmsghdr*>(buf);
  } while (nlh->nlmsg_type != AUDIT_USER_AVC);
  char* data = static_cast<char*>(NLMSG_DATA(nlh));
  EXPECT_NE(strstr(data, message.c_str()), nullptr);

  EXPECT_TRUE(UnregisterAuditDaemon(fd1.get()).is_ok());
}

TEST(NetlinkAuditTest, ForkedChildReadsParentsQueuedMessage) {
  fbl::unique_fd fd = OpenNetlinkAuditSocket();
  auto fork_helper = test_helper::ForkHelper();
  ASSERT_TRUE(fd.is_valid());

  const std::string message = "message_before_fork";
  ASSERT_TRUE(SendUserAuditMessage(fd.get(), 201, message).is_ok());
  ASSERT_TRUE(RegisterAsAuditDaemon(fd.get(), getpid()).is_ok());

  pid_t child_pid = fork_helper.RunInForkedProcess([&] {
    // Child process: try to read the message from the fd.
    char buf[NETLINK_BUF_SIZE]{};
    struct nlmsghdr* nlh;
    do {
      auto recv_res = ReceiveNetlinkMessage(fd.get(), buf, sizeof(buf));
      ASSERT_TRUE(recv_res.is_ok());
      ASSERT_GE(recv_res.value(), NLMSG_HDRLEN);
      nlh = reinterpret_cast<struct nlmsghdr*>(buf);
    } while (nlh->nlmsg_type != AUDIT_USER_AVC);

    char* data = static_cast<char*>(NLMSG_DATA(nlh));
    EXPECT_NE(strstr(data, message.c_str()), nullptr);
  });
  ASSERT_GE(child_pid, 0) << "fork failed: " << strerror(errno);

  // Parent process: wait for child and check its exit code.
  ASSERT_TRUE(fork_helper.WaitForChildren());
  EXPECT_TRUE(UnregisterAuditDaemon(fd.get()).is_ok());
}

TEST(NetlinkAuditTest, ChildDaemonReadsParentsMessageFromNewSocket) {
  fbl::unique_fd fd_parent_initial = OpenNetlinkAuditSocket();
  auto fork_helper = test_helper::ForkHelper();
  const std::string message = "message_from_parent_new_socket";
  ASSERT_TRUE(fd_parent_initial.is_valid());

  pid_t child_pid = fork_helper.RunInForkedProcess([&] {
    // Register as the audit daemon on the inherited socket.
    ASSERT_TRUE(RegisterAsAuditDaemon(fd_parent_initial.get(), getpid()).is_ok());
    // Child process: try to read the message from the fd.
    char buf[NETLINK_BUF_SIZE]{};
    struct nlmsghdr* nlh;
    do {
      auto recv_res = ReceiveNetlinkMessage(fd_parent_initial.get(), buf, sizeof(buf));
      ASSERT_TRUE(recv_res.is_ok());
      ASSERT_GE(recv_res.value(), NLMSG_HDRLEN);
      nlh = reinterpret_cast<struct nlmsghdr*>(buf);
    } while (nlh->nlmsg_type != AUDIT_USER_AVC);

    char* data = static_cast<char*>(NLMSG_DATA(nlh));
    EXPECT_NE(strstr(data, message.c_str()), nullptr);
    EXPECT_TRUE(UnregisterAuditDaemon(fd_parent_initial.get()).is_ok());
  });
  ASSERT_GE(child_pid, 0) << "fork failed: " << strerror(errno);

  // Close the original socket. The child now has the only reference.
  fd_parent_initial.reset();

  // Open a new socket and send the message.
  fbl::unique_fd fd_parent_new = OpenNetlinkAuditSocket();
  ASSERT_TRUE(fd_parent_new.is_valid());
  ASSERT_TRUE(SendUserAuditMessage(fd_parent_new.get(), 301, message).is_ok());

  ASSERT_TRUE(fork_helper.WaitForChildren());
}

TEST(NetlinkAuditTest, ForkedChildUnregistersParentOnOneSocket) {
  fbl::unique_fd fd = OpenNetlinkAuditSocket();
  auto fork_helper = test_helper::ForkHelper();
  ASSERT_TRUE(fd.is_valid());

  ASSERT_TRUE(RegisterAsAuditDaemon(fd.get(), getpid()).is_ok());

  pid_t child_pid = fork_helper.RunInForkedProcess([&] {
    // Child process: try to read the message from the fd.
    ASSERT_FALSE(UnregisterAuditDaemon(fd.get()).is_ok());
  });
  ASSERT_GE(child_pid, 0) << "fork failed: " << strerror(errno);

  // Parent process: wait for child and check its exit code.
  ASSERT_TRUE(fork_helper.WaitForChildren());
  EXPECT_TRUE(UnregisterAuditDaemon(fd.get()).is_ok());
}

TEST(NetlinkAuditTest, SocketCreateAllowed) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:audit_netlink_create_test_t:s0", [&] {
    EXPECT_THAT(socket(AF_NETLINK, SOCK_RAW, NETLINK_AUDIT), SyscallSucceeds())
        << "Netlink socket open should succeed";
  }));
}

TEST(NetlinkAuditTest, SocketCreateDenied) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:audit_netlink_restricted_test_t:s0", [&] {
    EXPECT_THAT(socket(AF_NETLINK, SOCK_RAW, NETLINK_AUDIT), SyscallFailsWithErrno(EACCES))
        << "Netlink socket open should fail";
  }));
}

struct SendTestParam {
  std::string_view label;
  bool should_wait_ack;
  int expected_errno;
};

class NetlinkAuditSendTest : public ::testing::TestWithParam<SendTestParam> {};

const char kCreateOnlyLabel[] = "test_u:test_r:audit_netlink_create_test_t:s0";
const char kReadLabel[] = "test_u:test_r:audit_netlink_read_test_t:s0";
const char kWriteLabel[] = "test_u:test_r:audit_netlink_write_test_t:s0";
const char kWriteRelayLabel[] = "test_u:test_r:audit_netlink_write_relay_test_t:s0";
const char kAllowWithWriteCapLabel[] = "test_u:test_r:audit_netlink_allow_with_write_cap_test_t:s0";
const char kAllowWithoutWriteCapLabel[] =
    "test_u:test_r:audit_netlink_allow_without_write_cap_test_t:s0";

TEST_P(NetlinkAuditSendTest, Send) {
  auto send_test = GetParam();
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs(send_test.label, [&] {
    fbl::unique_fd fd = OpenNetlinkAuditSocket();
    ASSERT_TRUE(fd.is_valid());

    auto result = SendUserAuditMessage(
        fd.get(), 1, std::string(send_test.label) + ": GENERIC_MESSAGE", send_test.should_wait_ack);
    // If the errno is expected to be 0, check for success.
    if (send_test.expected_errno == 0) {
      ASSERT_TRUE(result.is_ok()) << "Netlink send should succeed";
    } else {
      ASSERT_TRUE(result.is_error()) << "Netlink send should fail";
      ASSERT_EQ(result.error_value(), send_test.expected_errno)
          << "Returned error should be " << strerror(send_test.expected_errno);
    }
  }));
}

INSTANTIATE_TEST_SUITE_P(NetlinkAuditWriteTestSuite, NetlinkAuditSendTest,
                         ::testing::Values(SendTestParam{kCreateOnlyLabel, false, EACCES},
                                           SendTestParam{kReadLabel, false, EACCES},
                                           SendTestParam{kWriteLabel, false, EACCES},
                                           SendTestParam{kWriteRelayLabel, false, 0},
                                           SendTestParam{kAllowWithoutWriteCapLabel, true, EPERM},
                                           SendTestParam{kAllowWithWriteCapLabel, true, 0}));

}  // namespace
