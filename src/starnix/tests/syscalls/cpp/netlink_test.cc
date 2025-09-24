// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <arpa/inet.h>
#include <endian.h>
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
#include <linux/netlink.h>
#include <linux/rtnetlink.h>

#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {
class NetlinkTest : public ::testing::Test {
 public:
  void SetUp() override {
    nl_sock_.reset(socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE));
    ASSERT_TRUE(nl_sock_.is_valid()) << strerror(errno);
  }

  ssize_t SendMsg(test_helper::NetlinkEncoder& encoder) const {
    iovec iov = {};
    encoder.Finalize(iov);
    struct msghdr header = {};
    header.msg_iov = &iov;
    header.msg_iovlen = 1;
    return sendmsg(nl_sock_.get(), &header, 0);
  }

  void TearDown() override {
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
    const nlmsghdr* nlmsg = reinterpret_cast<const nlmsghdr*>(msg_buf_);
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
}

}  // namespace
