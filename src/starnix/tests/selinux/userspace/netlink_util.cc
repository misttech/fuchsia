// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/starnix/tests/selinux/userspace/netlink_util.h"

#include <sys/socket.h>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/audit.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>

#include "src/starnix/tests/selinux/userspace/util.h"

namespace netlink_util {

int SendNetlinkMsg(const netlink_util::NetlinkSocketTestCase& test_case) {
  auto sockcreate = ScopedTaskAttrResetter::SetTaskAttr("sockcreate", test_case.label);
  auto enforce = ScopedEnforcement::SetEnforcing();

  fbl::unique_fd sock_fd(socket(AF_NETLINK, SOCK_RAW, test_case.protocol));
  if (!sock_fd.is_valid()) {
    ADD_FAILURE() << "Creating socket failed with error:" << strerror(errno);
    return errno;
  }
  union {
    struct rtmsg rtmsg;
    struct audit_status audit_status;
  } payload;
  memset(&payload, 0, sizeof(payload));

  char buffer[NLMSG_ALIGN(sizeof(struct nlmsghdr) + sizeof(payload))];
  memset(buffer, 0, sizeof(buffer));

  struct nlmsghdr* nlh = (struct nlmsghdr*)buffer;
  nlh->nlmsg_type = test_case.message_type;
  nlh->nlmsg_flags = test_case.flags;
  nlh->nlmsg_pid = getpid();
  nlh->nlmsg_seq = 1;

  size_t payload_len = 0;
  if (test_case.protocol == NETLINK_AUDIT) {
    payload_len = sizeof(struct audit_status);
    nlh->nlmsg_len = NLMSG_LENGTH((__u32)payload_len);
    struct audit_status* audit_status = (struct audit_status*)NLMSG_DATA(nlh);
    audit_status->mask = AUDIT_STATUS_ENABLED | AUDIT_STATUS_PID;
    audit_status->enabled = 1;
    audit_status->pid = getpid();
  } else {
    payload_len = sizeof(struct rtmsg);
    nlh->nlmsg_len = NLMSG_LENGTH((__u32)payload_len);
    struct rtmsg* rt_msg = (struct rtmsg*)NLMSG_DATA(nlh);
    rt_msg->rtm_family = AF_INET;
    rt_msg->rtm_table = RT_TABLE_MAIN;
    rt_msg->rtm_protocol = RTPROT_STATIC;
    rt_msg->rtm_scope = RT_SCOPE_UNIVERSE;
    rt_msg->rtm_type = RTN_UNICAST;
  }

  struct sockaddr_nl sa;
  memset(&sa, 0, sizeof(sa));
  sa.nl_family = AF_NETLINK;

  return (int)sendto(sock_fd.get(), nlh, nlh->nlmsg_len, 0, (struct sockaddr*)&sa, sizeof(sa));
}

}  // namespace netlink_util
