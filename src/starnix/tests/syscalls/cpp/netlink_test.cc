// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <arpa/inet.h>
#include <endian.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/capability.h>
#include <linux/fib_rules.h>
#include <linux/if_link.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>

#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

class NetlinkTest : public ::testing::Test {
 public:
  void SetUp() override {
    if (!test_helper::HasCapability(CAP_NET_ADMIN)) {
      GTEST_SKIP() << "Needs CAP_NET_ADMIN";
    }

    nl_sock_.reset(socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE));
    ASSERT_TRUE(nl_sock_.is_valid()) << strerror(errno);
  }

  ssize_t SendMsg(test_helper::NetlinkEncoder &encoder) const {
    iovec iov = {};
    encoder.Finalize(iov);
    struct msghdr header = {};
    header.msg_iov = &iov;
    header.msg_iovlen = 1;
    return sendmsg(nl_sock_.get(), &header, 0);
  }

  void CheckNetlinkAlive() {
    // Check netlink is still there and responding to observe any kernel panics
    // happening on the netlink thread.
    test_helper::NetlinkEncoder encoder(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP);
    struct ifinfomsg ifi = {
        .ifi_family = AF_UNSPEC,
        .ifi_type = 0,
        .ifi_index = 0,
        .ifi_flags = 0,
        .ifi_change = 0,
    };
    encoder.Write(ifi);
    ASSERT_THAT(SendMsg(encoder), SyscallSucceeds());

    char msg_buf_[4096];
    ssize_t len = recv(nl_sock_.get(), msg_buf_, sizeof(msg_buf_), 0);
    ASSERT_GT(len, 0) << strerror(errno);
    const nlmsghdr *nlmsg = reinterpret_cast<const nlmsghdr *>(msg_buf_);
    ASSERT_TRUE(MY_NLMSG_OK(nlmsg, len));
    // Accept either NEWLINK or DONE from the system. If there are no links
    // installed, then DONE is seen instead of NEWLINK.
    ASSERT_THAT(nlmsg->nlmsg_type,
                testing::AnyOf(testing::Eq(RTM_NEWLINK), testing::Eq(NLMSG_DONE)));
  }

  fbl::unique_fd nl_sock_;
};

// Regression test for Syzkaller netlink crash in https://fxbug.dev/390646804.
//
// Crash was caused by the RTM_NEWPREFIX message being unimplemented and falling
// through a panic.
TEST_F(NetlinkTest, RtmNewPrefixDoesntCrash) {
  test_helper::NetlinkEncoder encoder(RTM_NEWPREFIX, 0);
  struct prefixmsg pfx = {};
  encoder.Write(pfx);
  ASSERT_THAT(SendMsg(encoder), SyscallSucceeds());
  CheckNetlinkAlive();
}

TEST_F(NetlinkTest, NoCapabilities) {
  test_helper::UnsetCapabilityEffective(CAP_NET_ADMIN);

  test_helper::NetlinkEncoder encoder(RTM_NEWRULE,
                                      NLM_F_REQUEST | NLM_F_CREATE | NLM_F_EXCL | NLM_F_ACK);

  // FIB Rule Header.
  fib_rule_hdr frh = {
      .family = AF_INET,         // IPv4 rule
      .dst_len = 0,              // Match any destination
      .src_len = 32,             // Match a /32 source address
      .table = RT_TABLE_UNSPEC,  // We will specify the table in an attribute
      .action = FR_ACT_TO_TBL,   // Action is to jump to a new table
  };
  encoder.Write(frh);

  // Add Attribute: Source Address (FRA_SRC)
  in_addr src_addr;
  inet_pton(AF_INET, "192.0.2.1", &src_addr);
  encoder.AddRtAttr(FRA_SRC, src_addr);

  // Add Attribute: Table ID (FRA_TABLE)
  uint32_t table_id = 100;
  encoder.AddRtAttr(FRA_TABLE, table_id);

  // Send the message.
  ASSERT_THAT(SendMsg(encoder), SyscallSucceeds());

  // Receive and verify the reply.
  char recv_buf[4096];
  ssize_t result;
  ASSERT_GT(result = recv(nl_sock_.get(), &recv_buf, sizeof(recv_buf), 0), 0) << strerror(errno);

  // Verify that we've received exactly one message that contains the `EPERM`
  // error.
  nlmsghdr *nlmsg = reinterpret_cast<nlmsghdr *>(recv_buf);
  uint32_t len = static_cast<uint32_t>(result);
  ASSERT_TRUE(NLMSG_OK(nlmsg, len));
  EXPECT_EQ(nlmsg->nlmsg_type, NLMSG_ERROR);
  nlmsgerr *err_data = reinterpret_cast<nlmsgerr *>(NLMSG_DATA(nlmsg));
  EXPECT_EQ(err_data->error, -EPERM);

  nlmsg = NLMSG_NEXT(nlmsg, len);
  EXPECT_FALSE(NLMSG_OK(nlmsg, len));

  // Restore the capability.
  test_helper::SetCapabilityEffective(CAP_NET_ADMIN);
}

}  // namespace
