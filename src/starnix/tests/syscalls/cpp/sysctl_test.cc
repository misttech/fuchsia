// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <arpa/inet.h>
#include <fcntl.h>
#include <net/if.h>
#include <netinet/in.h>
#include <string.h>
#include <sys/ioctl.h>

#include <algorithm>
#include <chrono>
#include <filesystem>
#include <format>
#include <thread>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <linux/capability.h>
#include <linux/if_tun.h>

#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

uint32_t GetLoopbackIndex() { return 1; }

// Waits for the address on the loopback device to be added or removed.
bool HasLoopbackAddress(int family, const char *address_str) {
  fbl::unique_fd nl_sock(socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE));
  EXPECT_TRUE(nl_sock.is_valid());

  test_helper::NetlinkEncoder encoder(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP);
  ifaddrmsg ifa_msg = {
      .ifa_family = AF_UNSPEC,
      .ifa_index = GetLoopbackIndex(),
  };
  encoder.Write(ifa_msg);

  iovec iov = {};
  encoder.Finalize(iov);
  struct msghdr msg = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
  };

  EXPECT_GE(sendmsg(nl_sock.get(), &msg, 0), 0) << strerror(errno);

  uint8_t addr[16];
  EXPECT_EQ(inet_pton(family, address_str, &addr), 1) << strerror(errno);

  char buf[8192];
  while (true) {
    ssize_t len = recv(nl_sock.get(), buf, sizeof(buf), 0);
    if (errno == EINTR) {
      continue;
    }
    EXPECT_GE(len, 0);
    for (nlmsghdr *nh = reinterpret_cast<nlmsghdr *>(buf); MY_NLMSG_OK(nh, len);
         nh = NLMSG_NEXT(nh, len)) {
      if (nh->nlmsg_type == NLMSG_DONE) {
        return false;
      }
      if (nh->nlmsg_type != RTM_NEWADDR) {
        continue;
      }

      ifaddrmsg *ifa = reinterpret_cast<ifaddrmsg *>(NLMSG_DATA(nh));
      if (ifa->ifa_family != family) {
        continue;
      }

      rtattr *rta = IFA_RTA(ifa);
      int rta_len = IFA_PAYLOAD(nh);
      for (; RTA_OK(rta, rta_len); rta = RTA_NEXT(rta, rta_len)) {
        if (rta->rta_type != IFA_ADDRESS) {
          continue;
        }
        if (memcmp(addr, RTA_DATA(rta), RTA_PAYLOAD(rta)) != 0) {
          continue;
        }
        // TODO(https://issues.fuchsia.dev/472336920): Netstack currently
        // only marks the address as unavailable which gets later translated
        // to tentative by netlink. We should report RTM_DELADDR if we find
        // it to be load bearing.
        if (test_helper::IsStarnix() && (ifa->ifa_flags & IFA_F_TENTATIVE)) {
          continue;
        }
        return true;
      }
    }
  }
}

// Creates a new TUN device with the given name.
fbl::unique_fd NewTunDevice(const char *name) {
  int tun = open("/dev/tun", O_RDWR);
  if (tun == -1 && errno == ENOENT) {
    tun = open("/dev/net/tun", O_RDWR);
  }
  EXPECT_GT(tun, 0) << strerror(errno);

  ifreq ifr{};
  ifr.ifr_flags = IFF_NO_PI | IFF_TUN;

  strncpy(ifr.ifr_name, name, IFNAMSIZ);
  EXPECT_EQ(ioctl(tun, TUNSETIFF, &ifr), 0) << strerror(errno);

  return fbl::unique_fd(tun);
}

class SysctlTest : public ::testing::Test {};

class SysctlTestWithParam
    : public SysctlTest,
      public ::testing::WithParamInterface<std::tuple<std::string, std::string>> {};

TEST_P(SysctlTestWithParam, DirectoryContainsInterfaces) {
  auto const &[version, conf_or_neigh] = GetParam();
  std::vector<std::string> files;
  EXPECT_TRUE(
      files::ReadDirContents(std::format("/proc/sys/net/{}/{}", version, conf_or_neigh), &files));
  EXPECT_THAT(files, testing::IsSupersetOf({"default", "lo"}));
}

INSTANTIATE_TEST_SUITE_P(SysctlTest, SysctlTestWithParam,
                         ::testing::Combine(::testing::Values("ipv4", "ipv6"),
                                            ::testing::Values("conf", "neigh")),
                         [](const ::testing::TestParamInfo<SysctlTestWithParam::ParamType> &info) {
                           return std::format("{}_{}", std::get<0>(info.param),
                                              std::get<1>(info.param));
                         });

TEST_F(SysctlTest, AcceptRaRtTable) {
  if (!test_helper::HasCapability(CAP_NET_ADMIN)) {
    GTEST_SKIP() << "Need CAP_NET_ADMIN to run SysctlTest";
  }
  std::string accept_ra_rt_table_str;

  constexpr const char *kAcceptRaRtTable = "/proc/sys/net/ipv6/conf/{}/accept_ra_rt_table";
  const std::string kDefault = std::format(kAcceptRaRtTable, "default");
  const std::string kLo = std::format(kAcceptRaRtTable, "lo");

  if (!test_helper::IsStarnix() && access(kDefault.c_str(), F_OK) == -1) {
    GTEST_SKIP() << "The kernel is not compiled with this sysctl";
  }

  const char *kVal1 = "-100\n";
  const char *kVal2 = "-200\n";

  for (auto const &path : {kDefault, kLo}) {
    EXPECT_TRUE(files::ReadFileToString(path, &accept_ra_rt_table_str));
    EXPECT_STREQ(accept_ra_rt_table_str.c_str(), "0\n");
  }

  // Write then read back value for interface lo.
  EXPECT_TRUE(files::WriteFile(kLo, kVal1));
  EXPECT_TRUE(files::ReadFileToString(kLo, &accept_ra_rt_table_str));
  EXPECT_STREQ(accept_ra_rt_table_str.c_str(), kVal1);

  // Write then read back value for special file `default`.
  EXPECT_TRUE(files::WriteFile(kDefault, kVal2));
  EXPECT_TRUE(files::ReadFileToString(kDefault, &accept_ra_rt_table_str));
  EXPECT_STREQ(accept_ra_rt_table_str.c_str(), kVal2);

  const char *kTunName = "tun0";
  auto tun = NewTunDevice(kTunName);

  const std::string kTunPath = std::format(kAcceptRaRtTable, kTunName);
  int trial = 0;
  while (!files::ReadFileToString(kTunPath, &accept_ra_rt_table_str)) {
    std::this_thread::sleep_for(std::chrono::milliseconds(100));
    ASSERT_LE(++trial, 100);
  }

  // The new device will have the `default` value.
  EXPECT_STREQ(accept_ra_rt_table_str.c_str(), kVal2);
}

TEST_F(SysctlTest, DisableIpv6) {
  if (!test_helper::HasCapability(CAP_NET_ADMIN)) {
    GTEST_SKIP() << "Need CAP_NET_ADMIN to run SysctlTest";
  }

  ASSERT_TRUE(HasLoopbackAddress(AF_INET6, "::1"));

  const char kDisableIpv6[] = "/proc/sys/net/ipv6/conf/lo/disable_ipv6";
  ASSERT_TRUE(files::WriteFile(kDisableIpv6, "1"));

  // IP configurations are applied synchronously at netstack, but netlink
  // watches for changes and updates addresses asynchronously. So we add some
  // retry to avoid flakiness. The maximum delay is 100 * 100ms = 10s.
  constexpr int kMaxAttempts = 100;
  constexpr std::chrono::milliseconds kRetryTimeout(100);

  // Verify ::1 is gone.
  bool removed = false;
  for (int trial = 0; trial < kMaxAttempts; trial++) {
    if (!HasLoopbackAddress(AF_INET6, "::1")) {
      removed = true;
      break;
    }
    std::this_thread::sleep_for(kRetryTimeout);
  }
  ASSERT_TRUE(removed) << "::1 is not gone after "
                       << kMaxAttempts * kRetryTimeout / std::chrono::seconds(1) << "s";

  // Re-enable it and assert ::1 is back.
  ASSERT_TRUE(files::WriteFile(kDisableIpv6, "0"));
  bool added = false;
  for (int trial = 0; trial < kMaxAttempts; trial++) {
    if (HasLoopbackAddress(AF_INET6, "::1")) {
      added = true;
      break;
    }
    std::this_thread::sleep_for(kRetryTimeout);
  }
  ASSERT_TRUE(added) << "::1 is not back after "
                     << kMaxAttempts * kRetryTimeout / std::chrono::seconds(1) << "s";
}

TEST_F(SysctlTest, DisableIpv6Default) {
  if (!test_helper::HasCapability(CAP_NET_ADMIN)) {
    GTEST_SKIP() << "Need CAP_NET_ADMIN to run SysctlTest";
  }

  files::WriteFile("/proc/sys/net/ipv6/conf/default/disable_ipv6", "1");
  auto tun = NewTunDevice("tun1");
  std::string disable_ipv6_str;
  int trial = 0;
  while (!files::ReadFileToString("/proc/sys/net/ipv6/conf/tun1/disable_ipv6", &disable_ipv6_str)) {
    std::this_thread::sleep_for(std::chrono::milliseconds(100));
    ASSERT_LE(++trial, 100);
  }
  ASSERT_STREQ(disable_ipv6_str.c_str(), "1\n");
}

struct SysctlTestReadBackParam {
  std::string path;
  const char *value;
};

class SysctlTestReadBack : public SysctlTest,
                           public ::testing::WithParamInterface<SysctlTestReadBackParam> {};

TEST_P(SysctlTestReadBack, ReadBack) {
  if (!test_helper::HasCapability(CAP_NET_ADMIN)) {
    GTEST_SKIP() << "Need CAP_NET_ADMIN to run SysctlTestReadBack";
  }
  const auto &[path, value] = GetParam();
  std::string to_write = std::format("{}\n", value);
  ASSERT_TRUE(files::WriteFile(path, to_write)) << strerror(errno);
  std::string to_read;
  ASSERT_TRUE(files::ReadFileToString(path, &to_read)) << strerror(errno);
  ASSERT_EQ(to_read, to_write);
}

INSTANTIATE_TEST_SUITE_P(
    SysctlTest, SysctlTestReadBack,
    ::testing::Values(
        SysctlTestReadBackParam{"/proc/sys/net/ipv6/neigh/default/ucast_solicit", "3"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv4/neigh/default/ucast_solicit", "3"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv6/neigh/default/mcast_resolicit", "3"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv4/neigh/default/mcast_resolicit", "3"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv6/conf/default/dad_transmits", "1"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv6/neigh/default/base_reachable_time_ms", "2000"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv4/neigh/default/base_reachable_time_ms", "2000"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv6/neigh/default/retrans_time_ms", "2000"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv4/neigh/default/retrans_time_ms", "2000"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv6/conf/default/use_tempaddr", "0"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv6/conf/default/use_tempaddr", "2"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv6/conf/default/accept_ra_defrtr", "0"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv6/conf/default/accept_ra_defrtr", "1"},
        SysctlTestReadBackParam{"/proc/sys/net/ipv4/tcp_rmem", "4096\t87380\t6291456"}),
    [](const testing::TestParamInfo<SysctlTestReadBackParam> &info) {
      auto path = std::filesystem::path(info.param.path);
      auto name = path.filename().string();
      auto version = std::next(path.begin(), 4)->string();
      std::string value = info.param.value;
      std::ranges::replace(value, '\t', '_');
      return std::format("{}_{}_{}", version, name, value);
    });
}  // namespace
