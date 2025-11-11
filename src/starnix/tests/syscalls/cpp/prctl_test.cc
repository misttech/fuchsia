// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/prctl.h>
#include <sys/syscall.h>

#include <fstream>
#include <iostream>
#include <string>

#include <gtest/gtest.h>
#include <linux/capability.h>
#include <linux/prctl.h>
#include <linux/securebits.h>

#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

// These are missing from our sys/prctl.h.
#define PR_CAP_AMBIENT 47
#define PR_CAP_AMBIENT_IS_SET 1
#define PR_CAP_AMBIENT_RAISE 2
#define PR_CAP_AMBIENT_LOWER 3
#define PR_CAP_AMBIENT_CLEAR_ALL 4

namespace {

TEST(PrctlTest, SubReaperTest) {
  // TODO(https://fxbug.dev/42080141): Find out why this test does not work on host in CQ
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "This test does not work on Linux in CQ";
  }

  test_helper::ForkHelper helper;

  // Reap children.
  prctl(PR_SET_CHILD_SUBREAPER, 1);

  pid_t ancestor_pid = SAFE_SYSCALL(getpid());
  ASSERT_NE(1, ancestor_pid);
  pid_t parent_pid = SAFE_SYSCALL(getppid());
  ASSERT_NE(0, parent_pid);
  ASSERT_NE(ancestor_pid, parent_pid);

  helper.RunInForkedProcess([&] {
    // Fork again
    helper.RunInForkedProcess([&] {
      // Wait to be reparented.
      while (SAFE_SYSCALL(getppid()) != ancestor_pid) {
      }
    });
    // Parent return and makes the child an orphan.
  });

  // Expect that both child ends up being reaped to this process.
  for (size_t i = 0; i < 2; ++i) {
    EXPECT_GT(wait(nullptr), 0);
  }
}

TEST(PrctlTest, SecureBits) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    SAFE_SYSCALL_SKIP_ON_EPERM(prctl(PR_SET_SECUREBITS, SECBIT_NOROOT));
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_GET_SECUREBITS)), SECBIT_NOROOT);
    SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, SECBIT_KEEP_CAPS));
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_GET_SECUREBITS)), SECBIT_KEEP_CAPS);
  });
}

TEST(PrctlTest, Argv0SniffingIsUndetectableInUserspace) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    std::ifstream cmdline_file("/proc/self/cmdline");
    ASSERT_TRUE(cmdline_file.is_open());
    std::string argv0;
    std::getline(cmdline_file, argv0, '\0');

    std::string argv0Basename = argv0;
    argv0Basename.erase(argv0Basename.begin(),
                        argv0Basename.begin() + argv0Basename.find_last_of('/') + 1);

    char name[16];
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_GET_NAME, name)), 0);
    ASSERT_EQ(std::string(name), "starnix_prctl_t");
    // The comm should be a truncation of argv[0] to start
    ASSERT_THAT(argv0Basename, ::testing::StartsWith(name));

    // Set the comm to a suffix of argv0, this will cause Starnix's sniffing to use the full
    // argv[0] as the Fuchsia-side name.
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_SET_NAME, "prctl_test")), 0);
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_GET_NAME, name)), 0);
    // Userspace should still observe just the infix even if the Fuchsia-side has the full string.
    ASSERT_EQ(std::string(name), "prctl_test");
    ASSERT_THAT(argv0Basename, ::testing::HasSubstr(name));
  });
}

TEST(PrctlTest, DropCapabilities) {
  // TODO(https://fxbug.dev/42080141): Find out why this test does not work on host in CQ
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "This test does not work on Linux in CQ";
  }

  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_CAPBSET_READ, CAP_DAC_OVERRIDE)), true);
    ASSERT_EQ(SAFE_SYSCALL_SKIP_ON_EPERM(prctl(PR_CAPBSET_DROP, CAP_DAC_OVERRIDE)), 0);
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_CAPBSET_READ, CAP_DAC_OVERRIDE)), false);
  });
}

TEST(PrctlTest, AmbientCapabilitiesBasicOperations) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_LOWER, CAP_CHOWN, 0, 0)), 0);
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, CAP_CHOWN, 0, 0)), 0);

    ASSERT_EQ(
        SAFE_SYSCALL_SKIP_ON_EPERM(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, CAP_CHOWN, 0, 0)),
        0);  // Requires CAP_SETPCAP ?
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, CAP_CHOWN, 0, 0)), 1);

    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0)), 0);
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, CAP_CHOWN, 0, 0)), 0);
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, CAP_AUDIT_CONTROL, 0, 0)),
              0);
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, CAP_DAC_OVERRIDE, 0, 0)),
              0);
  });
}

class CapGetSetTest : public ::testing::Test {
 protected:
  void SetUp() override {
    memset(&header_, 0, sizeof(header_));
    memset(&caps_, 0, sizeof(caps_));
  }

  __user_cap_header_struct header_;
  __user_cap_data_struct caps_[_LINUX_CAPABILITY_U32S_3];
};

/// `capget` should succeed when the header is valid and the target
/// thread is the caller.
TEST_F(CapGetSetTest, CapGet) {
  // The calling process can be referenced using either a header
  // `pid` value of 0, or the caller's PID.
  header_.version = _LINUX_CAPABILITY_VERSION_3;
  header_.pid = 0;
  EXPECT_THAT(syscall(SYS_capget, &header_, &caps_), SyscallSucceeds());

  header_.pid = getpid();
  EXPECT_THAT(syscall(SYS_capget, &header_, &caps_), SyscallSucceeds());
}

/// `capget` should succeed when the header is valid and the target
/// thread is different from the caller.
TEST_F(CapGetSetTest, CapGetDifferentPid) {
  test_helper::ForkHelper helper;

  pid_t parent_pid = getpid();

  helper.RunInForkedProcess([&parent_pid, this] {
    header_.version = _LINUX_CAPABILITY_VERSION_3;
    header_.pid = parent_pid;
    EXPECT_THAT(syscall(SYS_capget, &header_, &caps_), SyscallSucceeds());
  });
}

/// `capget` populates the header version field with the preferred capability
/// version and fails with EINVAL when the header contains an invalid capability
/// version and the pointer to the capability data struct is non-null.
TEST_F(CapGetSetTest, CapGetInvalidCapabilityVersion) {
  EXPECT_THAT(syscall(SYS_capget, &header_, &caps_), SyscallFailsWithErrno(EINVAL));
  EXPECT_EQ(header_.version, static_cast<__u32>(_LINUX_CAPABILITY_VERSION_3));
}

/// `capget` fails with EINVAL when the header contains an invalid pid
/// and the pointer to the capability data struct is non-null.
TEST_F(CapGetSetTest, CapGetInvalidPid) {
  header_.version = _LINUX_CAPABILITY_VERSION_3;
  header_.pid = -1;
  EXPECT_THAT(syscall(SYS_capget, &header_, &caps_), SyscallFailsWithErrno(EINVAL));
}

/// `capget` fails with EINVAL when the header contains an invalid pid
/// and version, and the pointer to the capability data struct is non-null.
/// The header is updated with the preferred capability version.
/// TODO(https://fxbug.dev/452426191): Modify the header on Starnix in this case.
TEST_F(CapGetSetTest, CapGetInvalidVersionAndPid) {
  header_.pid = -1;
  EXPECT_THAT(syscall(SYS_capget, &header_, &caps_), SyscallFailsWithErrno(EINVAL));
  EXPECT_EQ(header_.version, static_cast<__u32>(_LINUX_CAPABILITY_VERSION_3));
}

/// `capget` fails with EFAULT when the pointer to the header argument is null,
/// whether or not the pointer to the capability data struct is non-null.
TEST_F(CapGetSetTest, CapGetNullHeader) {
  EXPECT_THAT(syscall(SYS_capget, NULL, &caps_), SyscallFailsWithErrno(EFAULT));
  EXPECT_THAT(syscall(SYS_capget, NULL, NULL), SyscallFailsWithErrno(EFAULT));
}

/// `capget` succeeds and does not modify the header's version field when
/// the provided header contains a valid but non-preferred capability version.
TEST_F(CapGetSetTest, CapGetNonPreferredHeaderVersion) {
  header_.version = _LINUX_CAPABILITY_VERSION_1;
  EXPECT_THAT(syscall(SYS_capget, &header_, &caps_), SyscallSucceeds());
  EXPECT_EQ(header_.version, static_cast<__u32>(_LINUX_CAPABILITY_VERSION_1));
}

/// `capget` succeeds and does not modify the header's version field when
/// the provided header contains a valid version and the pointer to the
/// capability data struct is null.
/// TODO(https://fxbug.dev/452426191): Don't modify the header on Starnix.
TEST_F(CapGetSetTest, CapGetNullUserDataAndValidHeaderVersion) {
  header_.version = _LINUX_CAPABILITY_VERSION_1;
  EXPECT_THAT(syscall(SYS_capget, &header_, NULL), SyscallSucceeds());
  EXPECT_EQ(header_.version, static_cast<__u32>(_LINUX_CAPABILITY_VERSION_1));
}

/// `capget` succeeds and populates the header's version field with the
/// preferred capability version when the header contains an invalid version
/// and a valid pid, and the pointer to the capability data struct is null.
TEST_F(CapGetSetTest, CapGetNullUserDataAndInvalidHeaderVersion) {
  EXPECT_THAT(syscall(SYS_capget, &header_, NULL), SyscallSucceeds());
  EXPECT_EQ(header_.version, static_cast<__u32>(_LINUX_CAPABILITY_VERSION_3));
}

/// `capget` succeeds and populates the header's version field with the
/// preferred capability version when the header contains an invalid version
/// and an invalid pid, and the pointer to the capability data struct is null.
TEST_F(CapGetSetTest, CapGetNullUserDataAndInvalidHeaderVersionAndPid) {
  header_.pid = -1;
  EXPECT_THAT(syscall(SYS_capget, &header_, NULL), SyscallSucceeds());
  EXPECT_EQ(header_.version, static_cast<__u32>(_LINUX_CAPABILITY_VERSION_3));
}

/// `capset` should succeed when the target PID is the same as the caller's,
/// the header version is valid, and the new capability set is consistent
/// with the rules described in https://man7.org/linux/man-pages/man2/capget.2.html.
TEST_F(CapGetSetTest, CapSet) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([this] {
    header_.version = _LINUX_CAPABILITY_VERSION_3;

    header_.pid = 0;
    EXPECT_THAT(syscall(SYS_capset, &header_, &caps_), SyscallSucceeds());

    header_.pid = getpid();
    EXPECT_THAT(syscall(SYS_capset, &header_, &caps_), SyscallSucceeds());
  });
}

// Attempting to set capabilities on a thread other than the caller should
// cause `capset` to fail with `EPERM`. For kernels that support file capabilities,
// this is true whether or not the caller has the `CAP_SETPCAP` capability;
// otherwise, callers with `CAP_SETPCAP` may set capabilities for other threads.
// A negative PID should result in either success (in the case that file caps are
// not supported, and assuming sufficient privilege) or `EPERM`.
//
// TODO(https://fxbug.dev/453731091): Include a test involving `CAP_SETPCAP`
// once we have decided which behavior we need, and test success cases for negative
// PIDs if file caps are not supported.
TEST_F(CapGetSetTest, CapSetDifferentPid) {
  test_helper::ForkHelper helper;

  pid_t parent_pid = getpid();

  helper.RunInForkedProcess([&parent_pid, this] {
    header_.version = _LINUX_CAPABILITY_VERSION_3;
    header_.pid = parent_pid;
    EXPECT_THAT(syscall(SYS_capset, &header_, &caps_), SyscallFailsWithErrno(EPERM));

    header_.pid = -1;
    EXPECT_THAT(syscall(SYS_capset, &header_, &caps_), SyscallFailsWithErrno(EPERM));
  });
}

/// `capset` populates the header version field with the preferred capability
/// version and fails with EINVAL when the header contains an invalid capability
/// version and the pointer to the capability data struct is non-null.
TEST_F(CapGetSetTest, CapSetInvalidCapabilityVersion) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([this] {
    EXPECT_THAT(syscall(SYS_capset, &header_, &caps_), SyscallFailsWithErrno(EINVAL));
    EXPECT_EQ(header_.version, static_cast<__u32>(_LINUX_CAPABILITY_VERSION_3));
  });
}

/// `capset` populates the header version field with the preferred capability
/// version and fails with EINVAL when the header contains an invalid capability
/// version and the target PID is different from the caller's PID.
/// TODO(https://fxbug.dev/452426191): Modify the header on Starnix in this case.
TEST_F(CapGetSetTest, CapSetInvalidCapabilityVersionAndDifferentPid) {
  test_helper::ForkHelper helper;

  pid_t parent_pid = getpid();

  helper.RunInForkedProcess([&parent_pid, this] {
    header_.pid = parent_pid;
    EXPECT_THAT(syscall(SYS_capset, &header_, &caps_), SyscallFailsWithErrno(EINVAL));
    EXPECT_EQ(header_.version, static_cast<__u32>(_LINUX_CAPABILITY_VERSION_3));
  });
}

/// `capset` fails with `EFAULT` when the provided header is valid and the
/// pointer to the capability data is null.
TEST_F(CapGetSetTest, CapSetNullData) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([this] {
    header_.version = _LINUX_CAPABILITY_VERSION_3;
    EXPECT_THAT(syscall(SYS_capset, &header_, NULL), SyscallFailsWithErrno(EFAULT));
  });
}

/// `capset` fails with `EINVAL` when the provided header has an invalid
/// capability version and the pointer to the capability data is null.
/// The header version is set to the preferred capability version.
TEST_F(CapGetSetTest, CapSetInvalidCapabilityVersionAndNullData) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([this] {
    EXPECT_THAT(syscall(SYS_capset, &header_, NULL), SyscallFailsWithErrno(EINVAL));
    EXPECT_EQ(header_.version, static_cast<__u32>(_LINUX_CAPABILITY_VERSION_3));
  });
}

/// `capset` fails with `EINVAL` when the provided header has a non-permitted
/// target PID and the pointer to the capability data is null.
TEST_F(CapGetSetTest, CapSetDifferentPidAndNullData) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([this] {
    header_.version = _LINUX_CAPABILITY_VERSION_3;
    header_.pid = -1;
    EXPECT_THAT(syscall(SYS_capset, &header_, NULL), SyscallFailsWithErrno(EPERM));
  });
}

// `capset` should fail with `EPERM` on attempts to add a new capability to the
// permitted set.
TEST_F(CapGetSetTest, CapSetExpandPermittedSet) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([this] {
    header_.version = _LINUX_CAPABILITY_VERSION_3;
    ASSERT_THAT(syscall(SYS_capget, &header_, &caps_), SyscallSucceeds());

    // Drop the `CAP_SYS_ADMIN` capability from the effective and permitted sets.
    caps_[CAP_TO_INDEX(CAP_SYS_ADMIN)].effective &= ~CAP_TO_MASK(CAP_SYS_ADMIN);
    caps_[CAP_TO_INDEX(CAP_SYS_ADMIN)].permitted &= ~CAP_TO_MASK(CAP_SYS_ADMIN);
    ASSERT_THAT(syscall(SYS_capset, &header_, &caps_), SyscallSucceeds());

    // Attempt to add the `CAP_SYS_ADMIN` capability back to the permitted set.
    caps_[CAP_TO_INDEX(CAP_SYS_ADMIN)].permitted |= CAP_TO_MASK(CAP_SYS_ADMIN);
    EXPECT_THAT(syscall(SYS_capset, &header_, &caps_), SyscallFailsWithErrno(EPERM));

    // The same request with an invalid `version` field in the header should
    // result in failure with `EINVAL`.
    header_.version = 0;
    EXPECT_THAT(syscall(SYS_capset, &header_, &caps_), SyscallFailsWithErrno(EINVAL));
    EXPECT_EQ(header_.version, static_cast<__u32>(_LINUX_CAPABILITY_VERSION_3));
  });
}

}  // namespace
