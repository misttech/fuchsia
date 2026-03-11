// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <netinet/ip.h>
#include <sys/socket.h>
#include <sys/un.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/audit.h>
#include <linux/if_ether.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <linux/sock_diag.h>

#include "src/lib/files/file.h"
#include "src/starnix/tests/selinux/userspace/netlink_util.h"
#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

extern std::string DoPrePolicyLoadWork() { return "socket_policy.pp"; }

namespace {

constexpr int kTestBacklog = 5;

std::string SocketFamilyToString(int family) {
  switch (family) {
    case AF_UNIX:
      return "AF_UNIX";
    case AF_INET:
      return "AF_INET";
    case AF_INET6:
      return "AF_INET6";
    case AF_NETLINK:
      return "AF_NETLINK";
    case AF_PACKET:
      return "AF_PACKET";
    default:
      return std::to_string(family);
  }
}

std::string SocketTypeToString(int type) {
  switch (type) {
    case SOCK_STREAM:
      return "SOCK_STREAM";
    case SOCK_DGRAM:
      return "SOCK_DGRAM";
    case SOCK_RAW:
      return "SOCK_RAW";
    default:
      return std::to_string(type);
  }
}

std::string SocketProtocolToString(int family, int protocol) {
  if (family == AF_INET && protocol == IPPROTO_ICMP) {
    return "IPPROTO_ICMP";
  }
  if (family == AF_PACKET && protocol == htons(ETH_P_ALL)) {
    return "ETH_P_ALL";
  }
  if (family == AF_NETLINK) {
    switch (protocol) {
      case NETLINK_ROUTE:
        return "NETLINK_ROUTE";
      case NETLINK_USERSOCK:
        return "NETLINK_USERSOCK";
      case NETLINK_AUDIT:
        return "NETLINK_AUDIT";
      case NETLINK_SOCK_DIAG:
        return "NETLINK_SOCK_DIAG";
      default:
        return std::to_string(protocol);
    }
  }
  return std::to_string(protocol);
}

std::string NetlinkMessageToString(int protocol, uint16_t type) {
  if (protocol == NETLINK_ROUTE) {
    switch (type) {
      case RTM_GETROUTE:
        return "RTM_GETROUTE";
      case RTM_GETNEIGH:
        return "RTM_GETNEIGH";
      case RTM_GETLINK:
        return "RTM_GETLINK";
      case RTM_NEWROUTE:
        return "RTM_NEWROUTE";
    }
  } else if (protocol == NETLINK_AUDIT) {
    switch (type) {
      case AUDIT_GET:
        return "AUDIT_GET";
      case AUDIT_GET_FEATURE:
        return "AUDIT_GET_FEATURE";
      case AUDIT_LIST_RULES:
        return "AUDIT_LIST_RULES";
      case AUDIT_USER:
        return "AUDIT_USER";
      case AUDIT_FIRST_USER_MSG:
        return "AUDIT_FIRST_USER_MSG";
      case AUDIT_LAST_USER_MSG2:
        return "AUDIT_LAST_USER_MSG2";
      case AUDIT_TTY_SET:
        return "AUDIT_TTY_SET";
      case AUDIT_SET:
        return "AUDIT_SET";
      case AUDIT_SET_FEATURE:
        return "AUDIT_SET_FEATURE";
    }
  } else if (protocol == NETLINK_SOCK_DIAG) {
    switch (type) {
      case SOCK_DIAG_BY_FAMILY:
        return "SOCK_DIAG_BY_FAMILY";
    }
  }
  return std::to_string(type);
}

std::string NetlinkFlagsToString(uint16_t flags) {
  char buf[8];
  snprintf(buf, sizeof(buf), "0x%x", flags);
  return buf;
}

struct SocketTestCase {
  int domain;
  int type;
};

fit::result<int, fbl::unique_fd> SocketWithLabel(int domain, int type, int protocol,
                                                 std::string_view label) {
  auto sockcreate = ScopedTaskAttrResetter::SetTaskAttr("sockcreate", label);
  fbl::unique_fd fd(socket(domain, type, protocol));
  if (!fd) {
    return fit::error(errno);
  }
  return fit::ok(std::move(fd));
}

class SocketTest : public ::testing::TestWithParam<SocketTestCase> {};

std::string SocketTestName(const testing::TestParamInfo<SocketTestCase>& info) {
  return SocketFamilyToString(info.param.domain) + "__" + SocketTypeToString(info.param.type);
}

TEST_P(SocketTest, SocketTakesProcessLabel) {
  const SocketTestCase& test_case = GetParam();
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_test_no_trans_t:s0"), fit::ok());

  fbl::unique_fd sockfd = fbl::unique_fd(socket(test_case.domain, test_case.type, 0));
  ASSERT_TRUE(sockfd) << strerror(errno);
  EXPECT_EQ(GetLabel(sockfd.get()), "test_u:test_r:socket_test_no_trans_t:s0");
}

INSTANTIATE_TEST_SUITE_P(
    SocketTests, SocketTest,
    ::testing::Values(SocketTestCase{AF_UNIX, SOCK_STREAM}, SocketTestCase{AF_UNIX, SOCK_DGRAM},
                      SocketTestCase{AF_UNIX, SOCK_RAW}, SocketTestCase{AF_PACKET, SOCK_RAW},
                      SocketTestCase{AF_NETLINK, SOCK_RAW}, SocketTestCase{AF_INET, SOCK_STREAM},
                      SocketTestCase{AF_INET6, SOCK_DGRAM}),
    SocketTestName);

struct SocketTransitionTestCase {
  int domain;
  int type;
  int protocol;
  std::string_view expected_label_type;
};

// For AF_INET IPPROTO_ICMP sockets, update ping range to include current GID to allow creating
// sockets.
void MaybeUpdatePingRange(int family, int protocol) {
  constexpr char kProcPingGroupRange[] = "/proc/sys/net/ipv4/ping_group_range";
  if (family != AF_INET || protocol != IPPROTO_ICMP) {
    return;
  }
  std::string ping_group_range;
  if (!files::ReadFileToString(kProcPingGroupRange, &ping_group_range)) {
    fprintf(stderr, "Failed to read %s.\n", kProcPingGroupRange);
    return;
  }
  std::stringstream ss(ping_group_range);
  gid_t min_gid = 0, max_gid = 0;
  if (!(ss >> min_gid >> max_gid)) {
    fprintf(stderr, "Failed to parse GIDs from file content: %s\n", ping_group_range.c_str());
    return;
  }
  gid_t current_egid = getegid();
  if (current_egid < min_gid || current_egid > max_gid) {
    char buf[100] = {};
    sprintf(buf, "%d %d", current_egid, current_egid);
    files::WriteFile(kProcPingGroupRange, buf);
  }
}

class SocketTransitionTest : public ::testing::TestWithParam<SocketTransitionTestCase> {
 protected:
  void SetUp() override {
    const SocketTransitionTestCase& test_case = GetParam();
    MaybeUpdatePingRange(test_case.domain, test_case.protocol);
  }
};

std::string SocketTransitionTestName(const testing::TestParamInfo<SocketTransitionTestCase>& info) {
  return SocketFamilyToString(info.param.domain) + "__" + SocketTypeToString(info.param.type) +
         "__" + SocketProtocolToString(info.param.domain, info.param.protocol);
}

TEST_P(SocketTransitionTest, SocketLabelingAccountsForTransitions) {
  const SocketTransitionTestCase& test_case = GetParam();
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_test_t:s0"), fit::ok());

  fbl::unique_fd sockfd =
      fbl::unique_fd(socket(test_case.domain, test_case.type, test_case.protocol));
  ASSERT_TRUE(sockfd) << strerror(errno);
  EXPECT_EQ(GetLabel(sockfd.get()), MakeTestSecurityContext(test_case.expected_label_type));
}

INSTANTIATE_TEST_SUITE_P(
    SocketTransitionTests, SocketTransitionTest,
    ::testing::Values(
        SocketTransitionTestCase{AF_UNIX, SOCK_STREAM, 0, "unix_stream_socket_test_t"},
        SocketTransitionTestCase{AF_UNIX, SOCK_DGRAM, 0, "unix_dgram_socket_test_t"},
        // AF_UNIX SOCK_RAW sockets are treated as SOCK_DGRAM.
        SocketTransitionTestCase{AF_UNIX, SOCK_RAW, 0, "unix_dgram_socket_test_t"},
        SocketTransitionTestCase{AF_INET, SOCK_STREAM, 0, "tcp_socket_test_t"},
        SocketTransitionTestCase{AF_INET, SOCK_DGRAM, 0, "udp_socket_test_t"},
        SocketTransitionTestCase{AF_INET, SOCK_DGRAM, IPPROTO_ICMP, "rawip_socket_test_t"},
        SocketTransitionTestCase{AF_PACKET, SOCK_RAW, htons(ETH_P_ALL), "packet_socket_test_t"},
        SocketTransitionTestCase{AF_NETLINK, SOCK_RAW, NETLINK_ROUTE,
                                 "netlink_route_socket_test_t"},
        SocketTransitionTestCase{AF_NETLINK, SOCK_RAW, NETLINK_USERSOCK, "netlink_socket_test_t"}),
    SocketTransitionTestName);

TEST(SocketTest, SockFileLabelIsCorrect) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_test_t:s0"), fit::ok());

  fbl::unique_fd sockfd = fbl::unique_fd(socket(AF_UNIX, SOCK_STREAM, 0));
  ASSERT_TRUE(sockfd) << strerror(errno);

  struct sockaddr_un sock_addr;
  const char* kSockPath = "/tmp/test_sock_file";
  memset(&sock_addr, 0, sizeof(struct sockaddr_un));
  sock_addr.sun_family = AF_UNIX;
  strncpy(sock_addr.sun_path, kSockPath, sizeof(sock_addr.sun_path) - 1);
  unlink(kSockPath);
  ASSERT_THAT(bind(sockfd.get(), (struct sockaddr*)&sock_addr, sizeof(struct sockaddr_un)),
              SyscallSucceeds());

  EXPECT_EQ(GetLabel(sockfd.get()), "test_u:test_r:unix_stream_socket_test_t:s0");
  EXPECT_EQ(GetLabel(kSockPath), "test_u:object_r:sock_file_test_t:s0");
}

TEST(SocketTest, ListenAllowed) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_listen_test_t:s0"), fit::ok());
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto sockfd = SocketWithLabel(AF_INET, SOCK_STREAM, 0, "test_u:test_r:socket_listen_yes_t:s0");
  ASSERT_TRUE(sockfd.is_ok()) << sockfd.error_value();

  sockaddr_in addr;
  std::memset(&addr, 0, sizeof(addr));
  addr.sin_family = AF_INET;
  addr.sin_addr.s_addr = INADDR_ANY;
  ASSERT_THAT(bind(sockfd.value().get(), (struct sockaddr*)&addr, sizeof(addr)), SyscallSucceeds());
  EXPECT_THAT(listen(sockfd.value().get(), kTestBacklog), SyscallSucceeds());
}

TEST(SocketTest, ListenDenied) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_listen_test_t:s0"), fit::ok());
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto sockfd = SocketWithLabel(AF_INET, SOCK_STREAM, 0, "test_u:test_r:socket_listen_no_t:s0");
  ASSERT_TRUE(sockfd.is_ok()) << sockfd.error_value();

  sockaddr_in addr;
  std::memset(&addr, 0, sizeof(addr));
  addr.sin_family = AF_INET;
  addr.sin_addr.s_addr = INADDR_ANY;
  ASSERT_THAT(bind(sockfd.value().get(), (struct sockaddr*)&addr, sizeof(addr)), SyscallSucceeds());
  EXPECT_THAT(listen(sockfd.value().get(), kTestBacklog), SyscallFailsWithErrno(EACCES));
}

TEST(SocketTest, SendmsgAllowed) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_sendmsg_test_t:s0"), fit::ok());
  auto sockcreate =
      ScopedTaskAttrResetter::SetTaskAttr("sockcreate", "test_u:test_r:socket_sendmsg_yes_t:s0");
  auto enforce = ScopedEnforcement::SetEnforcing();

  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));
  char data[] = "y";
  struct iovec iov[] = {{
      .iov_base = data,
      .iov_len = 1,
  }};
  struct msghdr msg = {0};
  msg.msg_iov = iov;
  msg.msg_iovlen = 1;

  EXPECT_THAT(sendmsg(fds[0], &msg, 0), SyscallSucceeds());
}

TEST(SocketTest, SendmsgDenied) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_sendmsg_test_t:s0"), fit::ok());
  auto sockcreate =
      ScopedTaskAttrResetter::SetTaskAttr("sockcreate", "test_u:test_r:socket_sendmsg_no_t:s0");
  auto enforce = ScopedEnforcement::SetEnforcing();

  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));

  char data[] = "n";
  struct iovec iov[] = {{
      .iov_base = data,
      .iov_len = 1,
  }};
  struct msghdr msg = {0};
  msg.msg_iov = iov;
  msg.msg_iovlen = 1;

  EXPECT_THAT(sendmsg(fds[0], &msg, 0), SyscallFailsWithErrno(EACCES));
}

TEST(SocketTest, WriteAllowed) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_sendmsg_test_t:s0"), fit::ok());
  auto sockcreate =
      ScopedTaskAttrResetter::SetTaskAttr("sockcreate", "test_u:test_r:socket_sendmsg_yes_t:s0");
  auto enforce = ScopedEnforcement::SetEnforcing();

  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));
  char data[] = "y";
  EXPECT_THAT(write(fds[0], &data, 1), SyscallSucceeds());
}

TEST(SocketTest, WriteDenied) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_sendmsg_test_t:s0"), fit::ok());
  auto sockcreate =
      ScopedTaskAttrResetter::SetTaskAttr("sockcreate", "test_u:test_r:socket_sendmsg_no_t:s0");
  auto enforce = ScopedEnforcement::SetEnforcing();

  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));
  char data[] = "n";
  EXPECT_THAT(write(fds[0], &data, 1), SyscallFailsWithErrno(EACCES));
}

class NetlinkSocketTest : public ::testing::TestWithParam<netlink_util::NetlinkSocketTestCase> {};

std::string NetlinkSocketTestName(
    const testing::TestParamInfo<netlink_util::NetlinkSocketTestCase>& info) {
  return SocketProtocolToString(AF_NETLINK, info.param.protocol) + "__" +
         NetlinkMessageToString(info.param.protocol, info.param.message_type) + "__" +
         NetlinkFlagsToString(info.param.flags) + "__" + std::string(info.param.label_type);
}

TEST_P(NetlinkSocketTest, CheckNetlinkMsgPermission) {
  const netlink_util::NetlinkSocketTestCase& test_case = GetParam();
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:socket_sendmsg_test_t:s0", [test_case]() {
    int result = netlink_util::SendNetlinkMsg(test_case);

    if (test_case.expected_result.is_ok()) {
      EXPECT_THAT(result, SyscallSucceeds());
    } else {
      EXPECT_THAT(result, SyscallFailsWithErrno(test_case.expected_result.error_value()));
    }
  }));
}

INSTANTIATE_TEST_SUITE_P(
    NetlinkSocketTests, NetlinkSocketTest,
    ::testing::Values(
        // NETLINK_ROUTE test cases
        netlink_util::NetlinkSocketTestCase{NETLINK_ROUTE, RTM_GETROUTE, NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_nlmsg_read_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_ROUTE, RTM_GETROUTE, NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_no_nlmsg_t", fit::error(EACCES)},
        netlink_util::NetlinkSocketTestCase{NETLINK_ROUTE, RTM_GETROUTE, NLM_F_REQUEST | NLM_F_DUMP,
                                            "netlink_socket_nlmsg_read_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_ROUTE, RTM_GETROUTE, NLM_F_REQUEST | NLM_F_DUMP,
                                            "netlink_socket_no_nlmsg_t", fit::error(EACCES)},
        netlink_util::NetlinkSocketTestCase{NETLINK_ROUTE, RTM_GETNEIGH, NLM_F_REQUEST | NLM_F_DUMP,
                                            "netlink_socket_nlmsg_read_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_ROUTE, RTM_GETNEIGH, NLM_F_REQUEST | NLM_F_DUMP,
                                            "netlink_socket_no_nlmsg_t", fit::error(EACCES)},
        netlink_util::NetlinkSocketTestCase{NETLINK_ROUTE, RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_nlmsg_read_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_ROUTE, RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_no_nlmsg_t", fit::error(EACCES)},
        netlink_util::NetlinkSocketTestCase{NETLINK_ROUTE, RTM_NEWROUTE,
                                            NLM_F_REQUEST | NLM_F_CREATE | NLM_F_EXCL,
                                            "netlink_socket_nlmsg_write_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_ROUTE, RTM_NEWROUTE,
                                            NLM_F_REQUEST | NLM_F_CREATE | NLM_F_EXCL,
                                            "netlink_socket_no_nlmsg_t", fit::error(EACCES)},

        // NETLINK_AUDIT test cases
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_GET, NLM_F_REQUEST,
                                            "netlink_socket_nlmsg_read_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_GET, NLM_F_REQUEST,
                                            "netlink_socket_no_nlmsg_t", fit::error(EACCES)},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_GET_FEATURE, NLM_F_REQUEST,
                                            "netlink_socket_nlmsg_read_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_GET_FEATURE, NLM_F_REQUEST,
                                            "netlink_socket_no_nlmsg_t", fit::error(EACCES)},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_LIST_RULES,
                                            NLM_F_REQUEST | NLM_F_DUMP,
                                            "netlink_socket_nlmsg_readpriv_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_LIST_RULES,
                                            NLM_F_REQUEST | NLM_F_DUMP, "netlink_socket_no_nlmsg_t",
                                            fit::error(EACCES)},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_USER, NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_nlmsg_relay_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_USER, NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_no_nlmsg_t", fit::error(EACCES)},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_FIRST_USER_MSG,
                                            NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_nlmsg_relay_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_FIRST_USER_MSG,
                                            NLM_F_REQUEST | NLM_F_ACK, "netlink_socket_no_nlmsg_t",
                                            fit::error(EACCES)},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_LAST_USER_MSG2,
                                            NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_nlmsg_relay_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_LAST_USER_MSG2,
                                            NLM_F_REQUEST | NLM_F_ACK, "netlink_socket_no_nlmsg_t",
                                            fit::error(EACCES)},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_TTY_SET, NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_nlmsg_tty_audit_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_TTY_SET, NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_no_nlmsg_t", fit::error(EACCES)},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_SET, NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_nlmsg_write_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_SET, NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_no_nlmsg_t", fit::error(EACCES)},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_SET_FEATURE,
                                            NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_nlmsg_write_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_AUDIT, AUDIT_SET_FEATURE,
                                            NLM_F_REQUEST | NLM_F_ACK, "netlink_socket_no_nlmsg_t",
                                            fit::error(EACCES)},

        // NETLINK_SOCK_DIAG test cases
        netlink_util::NetlinkSocketTestCase{NETLINK_SOCK_DIAG, SOCK_DIAG_BY_FAMILY,
                                            NLM_F_REQUEST | NLM_F_ACK,
                                            "netlink_socket_nlmsg_read_t", fit::ok()},
        netlink_util::NetlinkSocketTestCase{NETLINK_SOCK_DIAG, SOCK_DIAG_BY_FAMILY,
                                            NLM_F_REQUEST | NLM_F_ACK, "netlink_socket_no_nlmsg_t",
                                            fit::error(EACCES)}),
    NetlinkSocketTestName);

TEST(SocketTest, RecvmsgAllowed) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_recvmsg_test_t:s0"), fit::ok());
  auto sockcreate =
      ScopedTaskAttrResetter::SetTaskAttr("sockcreate", "test_u:test_r:socket_recvmsg_yes_t:s0");
  auto enforce = ScopedEnforcement::SetEnforcing();

  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));
  char data[] = "y";
  struct iovec iov[] = {{
      .iov_base = data,
      .iov_len = 1,
  }};
  struct msghdr msg = {0};
  msg.msg_iov = iov;
  msg.msg_iovlen = 1;

  ASSERT_THAT(sendmsg(fds[0], &msg, 0), SyscallSucceeds());
  EXPECT_THAT(recvmsg(fds[1], &msg, 0), SyscallSucceeds());
}

TEST(SocketTest, RecvmsgDenied) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_recvmsg_test_t:s0"), fit::ok());
  auto sockcreate =
      ScopedTaskAttrResetter::SetTaskAttr("sockcreate", "test_u:test_r:socket_recvmsg_no_t:s0");
  auto enforce = ScopedEnforcement::SetEnforcing();

  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));

  char data[] = "n";
  struct iovec iov[] = {{
      .iov_base = data,
      .iov_len = 1,
  }};
  struct msghdr msg = {0};
  msg.msg_iov = iov;
  msg.msg_iovlen = 1;

  ASSERT_THAT(sendmsg(fds[0], &msg, 0), SyscallSucceeds());
  EXPECT_THAT(recvmsg(fds[1], &msg, 0), SyscallFailsWithErrno(EACCES));
}

TEST(SocketTest, ReadAllowed) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_recvmsg_test_t:s0"), fit::ok());
  auto sockcreate =
      ScopedTaskAttrResetter::SetTaskAttr("sockcreate", "test_u:test_r:socket_recvmsg_yes_t:s0");
  auto enforce = ScopedEnforcement::SetEnforcing();

  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));
  char data[] = "y";
  ASSERT_THAT(write(fds[0], &data, 1), SyscallSucceeds());
  EXPECT_THAT(read(fds[1], &data, 1), SyscallSucceeds());
}

TEST(SocketTest, ReadDenied) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_recvmsg_test_t:s0"), fit::ok());
  auto sockcreate =
      ScopedTaskAttrResetter::SetTaskAttr("sockcreate", "test_u:test_r:socket_recvmsg_no_t:s0");
  auto enforce = ScopedEnforcement::SetEnforcing();

  int fds[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds));
  char data[] = "y";
  ASSERT_THAT(write(fds[0], &data, 1), SyscallSucceeds());
  EXPECT_THAT(read(fds[1], &data, 1), SyscallFailsWithErrno(EACCES));
}

TEST(SocketTest, GetSocknameAndPeername) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_getname_test_t:s0"), fit::ok());
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto listen_fd =
      SocketWithLabel(AF_UNIX, SOCK_STREAM, 0, "test_u:test_r:socket_getname_yes_t:s0");
  ASSERT_TRUE(listen_fd.is_ok()) << listen_fd.error_value();
  auto client_fd = SocketWithLabel(AF_UNIX, SOCK_STREAM, 0, "test_u:test_r:socket_getname_no_t:s0");
  ASSERT_TRUE(client_fd.is_ok()) << client_fd.error_value();

  constexpr char kListenPath[] = "/tmp/getpeername_test";
  struct sockaddr_un sock_addr{.sun_family = AF_UNIX};
  strncpy(sock_addr.sun_path, kListenPath, sizeof(sock_addr.sun_path) - 1);
  ASSERT_THAT(bind(listen_fd.value().get(), (struct sockaddr*)&sock_addr, sizeof(sock_addr)),
              SyscallSucceeds());
  ASSERT_THAT(listen(listen_fd.value().get(), kTestBacklog), SyscallSucceeds());
  ASSERT_THAT(connect(client_fd.value().get(), (struct sockaddr*)&sock_addr, sizeof(sock_addr)),
              SyscallSucceeds());

  fbl::unique_fd accepted_fd;
  ASSERT_TRUE((accepted_fd = fbl::unique_fd(accept(listen_fd.value().get(), nullptr, nullptr))))
      << strerror(errno);
  sockaddr_in addr;
  socklen_t addr_len = sizeof(addr);
  std::memset(&addr, 0, sizeof(addr));

  EXPECT_THAT(getsockname(accepted_fd.get(), (struct sockaddr*)&addr, &addr_len),
              SyscallSucceeds());
  EXPECT_THAT(getpeername(accepted_fd.get(), (struct sockaddr*)&addr, &addr_len),
              SyscallSucceeds());
  EXPECT_THAT(getsockname(client_fd.value().get(), (struct sockaddr*)&addr, &addr_len),
              SyscallFailsWithErrno(EACCES));
  EXPECT_THAT(getpeername(client_fd.value().get(), (struct sockaddr*)&addr, &addr_len),
              SyscallFailsWithErrno(EACCES));
}

TEST(SocketTest, AcceptAllowed) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_accept_test_t:s0"), fit::ok());
  auto enforce = ScopedEnforcement::SetEnforcing();
  fbl::unique_fd listen_fd, client_fd;
  {
    auto sockcreate =
        ScopedTaskAttrResetter::SetTaskAttr("sockcreate", "test_u:test_r:socket_accept_yes_t:s0");
    ASSERT_TRUE((listen_fd = fbl::unique_fd(socket(AF_UNIX, SOCK_STREAM, 0)))) << strerror(errno);
  }

  ASSERT_TRUE((client_fd = fbl::unique_fd(socket(AF_UNIX, SOCK_STREAM, 0)))) << strerror(errno);
  constexpr char kListenPath[] = "/tmp/accept_test_yes";
  struct sockaddr_un sock_addr{.sun_family = AF_UNIX};
  strncpy(sock_addr.sun_path, kListenPath, sizeof(sock_addr.sun_path) - 1);
  ASSERT_THAT(bind(listen_fd.get(), (struct sockaddr*)&sock_addr, sizeof(sock_addr)),
              SyscallSucceeds());
  ASSERT_THAT(listen(listen_fd.get(), kTestBacklog), SyscallSucceeds());
  ASSERT_THAT(connect(client_fd.get(), (struct sockaddr*)&sock_addr, sizeof(sock_addr)),
              SyscallSucceeds());

  // Accept the connection in a domain that is only allowed the "accept" permission, to verify that
  // only the "accept" permission is required and that the "create" permission is not needed to
  // create `accepted_fd` on `accept()`.
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_accept_only_test_t:s0"), fit::ok());
  fbl::unique_fd accepted_fd;
  EXPECT_TRUE((accepted_fd = fbl::unique_fd(accept(listen_fd.get(), nullptr, nullptr))))
      << strerror(errno);
}

TEST(SocketTest, AcceptDenied) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_accept_test_t:s0"), fit::ok());
  auto enforce = ScopedEnforcement::SetEnforcing();
  fbl::unique_fd listen_fd, client_fd;
  {
    auto sockcreate =
        ScopedTaskAttrResetter::SetTaskAttr("sockcreate", "test_u:test_r:socket_accept_no_t:s0");
    ASSERT_TRUE((listen_fd = fbl::unique_fd(socket(AF_UNIX, SOCK_STREAM, 0)))) << strerror(errno);
  }

  ASSERT_TRUE((client_fd = fbl::unique_fd(socket(AF_UNIX, SOCK_STREAM, 0)))) << strerror(errno);
  constexpr char kListenPath[] = "/tmp/accept_test_no";
  struct sockaddr_un sock_addr{.sun_family = AF_UNIX};
  strncpy(sock_addr.sun_path, kListenPath, sizeof(sock_addr.sun_path) - 1);
  ASSERT_THAT(bind(listen_fd.get(), (struct sockaddr*)&sock_addr, sizeof(sock_addr)),
              SyscallSucceeds());
  ASSERT_THAT(listen(listen_fd.get(), kTestBacklog), SyscallSucceeds());
  ASSERT_THAT(connect(client_fd.get(), (struct sockaddr*)&sock_addr, sizeof(sock_addr)),
              SyscallSucceeds());

  fbl::unique_fd accepted_fd;
  EXPECT_THAT(accept(listen_fd.get(), nullptr, nullptr), SyscallFailsWithErrno(EACCES));
}

fit::result<int, std::string> GetPeerSec(int fd) {
  char label_buf[256]{};
  socklen_t label_len = sizeof(label_buf);
  if (getsockopt(fd, SOL_SOCKET, SO_PEERSEC, label_buf, &label_len) == -1) {
    return fit::error(errno);
  }
  return RemoveTrailingNul(std::string(label_buf, label_len));
}

TEST(SocketPeerSecTest, UnixDomainStream) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_test_t:s0"), fit::ok());

  auto listen_fd = SocketWithLabel(AF_UNIX, SOCK_STREAM, 0, "test_u:test_r:socket_test_peer_t:s0");
  ASSERT_TRUE(listen_fd.is_ok()) << listen_fd.error_value();
  EXPECT_THAT(GetLabel(listen_fd.value().get()), IsOk("test_u:test_r:socket_test_peer_t:s0"));

  // Before connecting, Unix stream sockets report the peer as the "unlabeled" context.
  EXPECT_THAT(GetPeerSec(listen_fd.value().get()), IsOk("unlabeled_u:unlabeled_r:unlabeled_t:s0"));

  fbl::unique_fd client_fd;
  ASSERT_TRUE((client_fd = fbl::unique_fd(socket(AF_UNIX, SOCK_STREAM, 0)))) << strerror(errno);
  EXPECT_THAT(GetLabel(client_fd.get()), IsOk("test_u:test_r:unix_stream_socket_test_t:s0"));
  EXPECT_THAT(GetPeerSec(client_fd.get()), IsOk("unlabeled_u:unlabeled_r:unlabeled_t:s0"));

  // Bind the `listen_fd` to an address and start listening on it.
  constexpr char kListenPath[] = "/tmp/unix_domain_stream_test";
  struct sockaddr_un sock_addr{.sun_family = AF_UNIX};
  strncpy(sock_addr.sun_path, kListenPath, sizeof(sock_addr.sun_path) - 1);
  ASSERT_THAT(bind(listen_fd.value().get(), (struct sockaddr*)&sock_addr, sizeof(sock_addr)),
              SyscallSucceeds());
  ASSERT_THAT(listen(listen_fd.value().get(), kTestBacklog), SyscallSucceeds());

  // Connect the `client_fd` to the listener, which should immediately cause the peer label to
  // reflect that of the listening socket.
  ASSERT_THAT(connect(client_fd.get(), (struct sockaddr*)&sock_addr, sizeof(sock_addr)),
              SyscallSucceeds());
  EXPECT_THAT(GetPeerSec(client_fd.get()), IsOk("test_u:test_r:socket_test_peer_t:s0"));

  // Accept the client connection on `listen_fd` and validate the peer label reported by the
  // accepted socket.
  fbl::unique_fd accepted_fd;
  ASSERT_TRUE((accepted_fd = fbl::unique_fd(accept(listen_fd.value().get(), nullptr, nullptr))))
      << strerror(errno);
  EXPECT_THAT(GetPeerSec(accepted_fd.get()), IsOk("test_u:test_r:unix_stream_socket_test_t:s0"));
}

TEST(SocketPeerSecTest, UnixDomainDatagram) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_test_t:s0"), fit::ok());

  fbl::unique_fd fd;
  ASSERT_TRUE((fd = fbl::unique_fd(socket(AF_UNIX, SOCK_DGRAM, 0)))) << strerror(errno);
  EXPECT_THAT(GetLabel(fd.get()), IsOk("test_u:test_r:unix_dgram_socket_test_t:s0"));

  // Unix datagram sockets do not support `SO_PEERSEC`.
  EXPECT_EQ(GetPeerSec(fd.get()), fit::error(ENOPROTOOPT));
}

TEST(SocketPeerSecTest, SocketPairUnixStream) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_test_t:s0"), fit::ok());

  int fds[2]{};
  ASSERT_THAT(socketpair(AF_UNIX, SOCK_STREAM, 0, fds), SyscallSucceeds());

  fbl::unique_fd fd1(fds[0]);
  fbl::unique_fd fd2(fds[1]);

  EXPECT_THAT(GetLabel(fd1.get()), IsOk("test_u:test_r:unix_stream_socket_test_t:s0"));
  EXPECT_THAT(GetLabel(fd2.get()), IsOk("test_u:test_r:unix_stream_socket_test_t:s0"));

  // Unix-domain sockets created with `socketpair()` should report each other's labels immediately.
  EXPECT_THAT(GetPeerSec(fd1.get()), IsOk("test_u:test_r:unix_stream_socket_test_t:s0"));
  EXPECT_THAT(GetPeerSec(fd2.get()), IsOk("test_u:test_r:unix_stream_socket_test_t:s0"));
}

TEST(SocketPeerSecTest, SocketPairUnixDatagram) {
  ASSERT_EQ(WriteTaskAttr("current", "test_u:test_r:socket_test_t:s0"), fit::ok());

  int fds[2];
  ASSERT_THAT(socketpair(AF_UNIX, SOCK_DGRAM, 0, fds), SyscallSucceeds());

  fbl::unique_fd fd1(fds[0]);
  fbl::unique_fd fd2(fds[1]);

  EXPECT_THAT(GetLabel(fd1.get()), IsOk("test_u:test_r:unix_dgram_socket_test_t:s0"));
  EXPECT_THAT(GetLabel(fd2.get()), IsOk("test_u:test_r:unix_dgram_socket_test_t:s0"));

  // Unix-domain datagram sockets created with `socketpair()` are described as supporting
  // `SO_PEERSEC` but actually seem to report not-supported.
  EXPECT_EQ(GetPeerSec(fd1.get()), fit::error(ENOPROTOOPT));
  EXPECT_EQ(GetPeerSec(fd2.get()), fit::error(ENOPROTOOPT));
}

}  // namespace
