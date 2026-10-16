// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <grp.h>
#include <signal.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>

#include <filesystem>
#include <fstream>
#include <iostream>
#include <string>
#include <thread>

#include <gtest/gtest.h>
#include <linux/capability.h>
#include <linux/prctl.h>
#include <linux/securebits.h>

#if defined(__x86_64__)
#include <asm/prctl.h>
#endif

#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
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

// Test that setreuid(-1,-1) does not spuriously update saved_uid.
// Bug: Starnix compared prev.uid against the raw euid argument (u32::MAX)
// instead of checking euid != -1 first, causing saved_uid to be set to
// the current euid on every setreuid(-1,-1) call.
TEST(PrctlTest, SetreuidNoopDoesNotCorruptSavedUid) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running as root";
  }
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    // Setup: uid=1000, euid=0, saved_uid=1000 (like a setuid-root binary)
    ASSERT_EQ(setresuid(1000, 0, 1000), 0);

    uid_t ruid, euid, suid;
    ASSERT_EQ(getresuid(&ruid, &euid, &suid), 0);
    ASSERT_EQ(ruid, 1000u);
    ASSERT_EQ(euid, 0u);
    ASSERT_EQ(suid, 1000u);

    // setreuid(-1, -1) should be a complete no-op
    ASSERT_EQ(setreuid(-1, -1), 0);
    ASSERT_EQ(getresuid(&ruid, &euid, &suid), 0);

    EXPECT_EQ(ruid, 1000u) << "setreuid(-1,-1) should not change ruid";
    EXPECT_EQ(euid, 0u) << "setreuid(-1,-1) should not change euid";
    EXPECT_EQ(suid, 1000u) << "setreuid(-1,-1) should not change saved_uid";
  });
}

// Full chain: setreuid(-1,-1) corrupts saved_uid → privilege escalation.
// Scenario: setuid-root binary with uid=1000, euid=0, saved_uid=1000
// calls setreuid(-1,-1) → saved_uid becomes 0 → drop euid → regain root.
TEST(PrctlTest, SetreuidNoopToPrivilegeEscalation) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running as root";
  }
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    uid_t ruid, euid, suid;

    // Step 1: Setuid-root binary state
    ASSERT_EQ(setresuid(1000, 0, 1000), 0);
    ASSERT_EQ(getresuid(&ruid, &euid, &suid), 0);
    ASSERT_EQ(ruid, 1000u);
    ASSERT_EQ(euid, 0u);
    ASSERT_EQ(suid, 1000u);

    // Step 2: No-op call - should NOT corrupt saved_uid
    ASSERT_EQ(setreuid(-1, -1), 0);
    ASSERT_EQ(getresuid(&ruid, &euid, &suid), 0);
    ASSERT_EQ(ruid, 1000u);
    ASSERT_EQ(euid, 0u);
    ASSERT_EQ(suid, 1000u);

    // Step 3: Drop euid
    ASSERT_EQ(seteuid(1000), 0);
    ASSERT_EQ(getresuid(&ruid, &euid, &suid), 0);
    ASSERT_EQ(ruid, 1000u);
    ASSERT_EQ(euid, 1000u);
    ASSERT_EQ(suid, 1000u);

    // Step 4: Try to regain root - must fail
    seteuid(0);
    ASSERT_EQ(getresuid(&ruid, &euid, &suid), 0);

    EXPECT_NE(euid, 0u) << "Should not regain root after privilege drop";
  });
}

// Test that setreuid() does not allow ruid=saved_uid.
// Linux's __sys_setreuid() only accepts ruid={current_uid, current_euid},
// deliberately excluding saved_uid. This prevents a process that dropped
// privileges from regaining root via setreuid when setresuid would be required.
TEST(PrctlTest, SetreuidRuidCannotUseSavedUid) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running as root";
  }
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    // Setup: uid=1000, euid=1000, saved_uid=0
    // Standard state after a setuid-root binary drops privileges
    ASSERT_EQ(setresuid(1000, 1000, 0), 0);

    uid_t ruid, euid, suid;
    ASSERT_EQ(getresuid(&ruid, &euid, &suid), 0);
    ASSERT_EQ(ruid, 1000u);
    ASSERT_EQ(euid, 1000u);
    ASSERT_EQ(suid, 0u);

    // setreuid(0, -1) - ruid=saved_uid. Linux denies this with EPERM.
    int ret = setreuid(0, -1);
    EXPECT_EQ(ret, -1) << "setreuid(saved_uid, -1) should be denied";
    if (ret == -1) {
      EXPECT_EQ(errno, EPERM);
    }

    // Verify credentials are unchanged
    ASSERT_EQ(getresuid(&ruid, &euid, &suid), 0);
    EXPECT_EQ(ruid, 1000u);
    EXPECT_EQ(euid, 1000u);
    EXPECT_EQ(suid, 0u);
  });
}

// Same test for setregid: rgid=saved_gid should be denied.
TEST(PrctlTest, SetregidRgidCannotUseSavedGid) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running as root";
  }
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    // Set up GID state first while still privileged: gid=1000, egid=1000, saved_gid=0.
    ASSERT_EQ(setresgid(1000, 1000, 0), 0);

    // Drop effective capabilities by transitioning euid from 0 to non-zero.
    // This mirrors a setuid-root binary that has dropped privileges, and in
    // particular clears CAP_SETGID so the setregid check below actually
    // exercises the saved_gid rule rather than being short-circuited by caps.
    ASSERT_EQ(setresuid(1000, 1000, 0), 0);

    gid_t rgid, egid, sgid;
    ASSERT_EQ(getresgid(&rgid, &egid, &sgid), 0);
    ASSERT_EQ(rgid, 1000u);
    ASSERT_EQ(egid, 1000u);
    ASSERT_EQ(sgid, 0u);

    // setregid(0, -1) - rgid=saved_gid. Linux denies this with EPERM.
    int ret = setregid(0, -1);
    EXPECT_EQ(ret, -1) << "setregid(saved_gid, -1) should be denied";
    if (ret == -1) {
      EXPECT_EQ(errno, EPERM);
    }

    // Verify credentials are unchanged
    ASSERT_EQ(getresgid(&rgid, &egid, &sgid), 0);
    EXPECT_EQ(rgid, 1000u);
    EXPECT_EQ(egid, 1000u);
    EXPECT_EQ(sgid, 0u);
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

TEST(PrctlTest, Dumpable) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    // PR_SET_DUMPABLE accepts 0 (disable) or 1 (user).
    EXPECT_THAT(prctl(PR_SET_DUMPABLE, 0), SyscallSucceedsWithValue(0));
    EXPECT_THAT(prctl(PR_GET_DUMPABLE), SyscallSucceedsWithValue(0));

    EXPECT_THAT(prctl(PR_SET_DUMPABLE, 1), SyscallSucceedsWithValue(0));
    EXPECT_THAT(prctl(PR_GET_DUMPABLE), SyscallSucceedsWithValue(1));
  });
}

TEST(PrctlTest, KeepCaps) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    // Initial keepcaps should be 0.
    EXPECT_THAT(prctl(PR_GET_KEEPCAPS), SyscallSucceedsWithValue(0));

    // Setting keepcaps to 1 should update both PR_GET_KEEPCAPS and PR_GET_SECUREBITS.
    EXPECT_THAT(prctl(PR_SET_KEEPCAPS, 1), SyscallSucceedsWithValue(0));
    EXPECT_THAT(prctl(PR_GET_KEEPCAPS), SyscallSucceedsWithValue(1));
    int securebits = SAFE_SYSCALL(prctl(PR_GET_SECUREBITS));
    EXPECT_TRUE(securebits & SECBIT_KEEP_CAPS);

    // Setting keepcaps to 0 should clear it.
    EXPECT_THAT(prctl(PR_SET_KEEPCAPS, 0), SyscallSucceedsWithValue(0));
    EXPECT_THAT(prctl(PR_GET_KEEPCAPS), SyscallSucceedsWithValue(0));
    securebits = SAFE_SYSCALL(prctl(PR_GET_SECUREBITS));
    EXPECT_FALSE(securebits & SECBIT_KEEP_CAPS);

    // Any value other than 0 or 1 should fail with EINVAL.
    EXPECT_THAT(prctl(PR_SET_KEEPCAPS, 2), SyscallFailsWithErrno(EINVAL));
    EXPECT_THAT(prctl(PR_SET_KEEPCAPS, -1), SyscallFailsWithErrno(EINVAL));
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(PrctlTest, KeepCapsLocked) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running as root";
  }
  test_helper::ForkHelper helper;

  // When SECBIT_KEEP_CAPS_LOCKED is set, PR_SET_KEEPCAPS fails with EPERM
  // regardless of whether SECBIT_KEEP_CAPS is changing or staying the same.
  helper.RunInForkedProcess([&] {
    SAFE_SYSCALL(setresuid(0, 0, 0));
    test_helper::SetCapabilityEffective(CAP_SETPCAP);

    ASSERT_THAT(prctl(PR_SET_SECUREBITS, SECBIT_KEEP_CAPS_LOCKED), SyscallSucceeds());
    EXPECT_THAT(prctl(PR_SET_KEEPCAPS, 1), SyscallFailsWithErrno(EPERM));
    EXPECT_THAT(prctl(PR_SET_KEEPCAPS, 0), SyscallFailsWithErrno(EPERM));
    EXPECT_THAT(prctl(PR_SET_KEEPCAPS, 2), SyscallFailsWithErrno(EINVAL));
    EXPECT_THAT(prctl(PR_GET_KEEPCAPS), SyscallSucceedsWithValue(0));
  });
  EXPECT_TRUE(helper.WaitForChildren());

  helper.RunInForkedProcess([&] {
    SAFE_SYSCALL(setresuid(0, 0, 0));
    test_helper::SetCapabilityEffective(CAP_SETPCAP);

    ASSERT_THAT(prctl(PR_SET_SECUREBITS, SECBIT_KEEP_CAPS | SECBIT_KEEP_CAPS_LOCKED),
                SyscallSucceeds());
    EXPECT_THAT(prctl(PR_SET_KEEPCAPS, 0), SyscallFailsWithErrno(EPERM));
    EXPECT_THAT(prctl(PR_SET_KEEPCAPS, 1), SyscallFailsWithErrno(EPERM));
    EXPECT_THAT(prctl(PR_GET_KEEPCAPS), SyscallSucceedsWithValue(1));
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(PrctlTest, NoNewPrivs) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    // Initially no_new_privs is not enabled.
    EXPECT_THAT(prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0), SyscallSucceedsWithValue(0));

    // PR_GET_NO_NEW_PRIVS with non-zero extra arguments should return EINVAL.
    EXPECT_THAT(prctl(PR_GET_NO_NEW_PRIVS, 1, 0, 0, 0), SyscallFailsWithErrno(EINVAL));

    // PR_SET_NO_NEW_PRIVS with arg2 != 1 should fail with EINVAL.
    EXPECT_THAT(prctl(PR_SET_NO_NEW_PRIVS, 0, 0, 0, 0), SyscallFailsWithErrno(EINVAL));
    EXPECT_THAT(prctl(PR_SET_NO_NEW_PRIVS, 2, 0, 0, 0), SyscallFailsWithErrno(EINVAL));

    // PR_SET_NO_NEW_PRIVS with extra non-zero arguments should fail with EINVAL.
    EXPECT_THAT(prctl(PR_SET_NO_NEW_PRIVS, 1, 1, 0, 0), SyscallFailsWithErrno(EINVAL));

    // Successfully enable no_new_privs.
    EXPECT_THAT(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), SyscallSucceedsWithValue(0));
    EXPECT_THAT(prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0), SyscallSucceedsWithValue(1));
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

TEST(PrctlTest, SecurebitNoCapAmbientRaisePreventsRaisingAmbientCaps) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running as root";
  }
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&]() {
    SAFE_SYSCALL(setresuid(0, 0, 0));

    test_helper::SetCapabilityEffective(CAP_SETPCAP);
    test_helper::SetCapabilityInheritable(CAP_AUDIT_READ);
    test_helper::SetCapabilityPermitted(CAP_AUDIT_READ);

    SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, SECBIT_NO_CAP_AMBIENT_RAISE));

    EXPECT_THAT(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, CAP_AUDIT_READ, 0, 0),
                SyscallFailsWithErrno(EPERM));
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

struct SecurebitParam {
  int bit;
  int lock_bit;
  const char *name;
};

class SecurebitsLockedTest : public ::testing::TestWithParam<SecurebitParam> {};

TEST_P(SecurebitsLockedTest, LockedBitPreventsSettingBit) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running as root";
  }
  SecurebitParam param = GetParam();
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&]() {
    SAFE_SYSCALL(setresuid(0, 0, 0));
    test_helper::SetCapabilityEffective(CAP_SETPCAP);

    SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, param.lock_bit));

    EXPECT_THAT(prctl(PR_SET_SECUREBITS, param.lock_bit | param.bit), SyscallFailsWithErrno(EPERM));

    EXPECT_EQ(SAFE_SYSCALL(prctl(PR_GET_SECUREBITS)), param.lock_bit);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST_P(SecurebitsLockedTest, LockedBitPreventsClearingBit) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running as root";
  }
  SecurebitParam param = GetParam();
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&]() {
    SAFE_SYSCALL(setresuid(0, 0, 0));
    test_helper::SetCapabilityEffective(CAP_SETPCAP);

    SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, param.bit));
    SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, param.bit | param.lock_bit));

    EXPECT_THAT(prctl(PR_SET_SECUREBITS, param.lock_bit), SyscallFailsWithErrno(EPERM));

    EXPECT_EQ(SAFE_SYSCALL(prctl(PR_GET_SECUREBITS)), param.bit | param.lock_bit);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST_P(SecurebitsLockedTest, LockedBitCannotBeCleared) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running as root";
  }
  SecurebitParam param = GetParam();
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&]() {
    SAFE_SYSCALL(setresuid(0, 0, 0));
    test_helper::SetCapabilityEffective(CAP_SETPCAP);

    SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, param.lock_bit));

    // Setting the same locked value again succeeds.
    EXPECT_THAT(prctl(PR_SET_SECUREBITS, param.lock_bit), SyscallSucceeds());

    // Try to clear the lock bit (by setting securebits to 0).
    EXPECT_THAT(prctl(PR_SET_SECUREBITS, 0), SyscallFailsWithErrno(EPERM));

    // Verify the lock bit is still set.
    EXPECT_EQ(SAFE_SYSCALL(prctl(PR_GET_SECUREBITS)), param.lock_bit);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

INSTANTIATE_TEST_SUITE_P(
    AllBits, SecurebitsLockedTest,
    ::testing::Values(SecurebitParam{SECBIT_NOROOT, SECBIT_NOROOT_LOCKED, "NOROOT"},
                      SecurebitParam{SECBIT_NO_SETUID_FIXUP, SECBIT_NO_SETUID_FIXUP_LOCKED,
                                     "NO_SETUID_FIXUP"},
                      SecurebitParam{SECBIT_KEEP_CAPS, SECBIT_KEEP_CAPS_LOCKED, "KEEP_CAPS"},
                      SecurebitParam{SECBIT_NO_CAP_AMBIENT_RAISE,
                                     SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED, "NO_CAP_AMBIENT_RAISE"}),
    [](const ::testing::TestParamInfo<SecurebitsLockedTest::ParamType> &info) {
      return info.param.name;
    });

TEST(PrctlTest, SecurebitsInvalidFlagsAndCapabilities) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running as root";
  }
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&]() {
    SAFE_SYSCALL(setresuid(0, 0, 0));
    test_helper::SetCapabilityEffective(CAP_SETPCAP);

    // Unrecognized bits in the lower 32 bits fail with EPERM.
    EXPECT_THAT(prctl(PR_SET_SECUREBITS, 1UL << 30, 0, 0, 0), SyscallFailsWithErrno(EPERM));
    EXPECT_THAT(prctl(PR_GET_SECUREBITS), SyscallSucceedsWithValue(0));

    // Unrecognized bits in the upper 32 bits (with valid lower 32 bits) must not be
    // truncated to 32 bits on 64-bit platforms and must fail with EPERM.
    if (sizeof(unsigned long) > 4) {
      unsigned long high_bit = static_cast<unsigned long>(1ULL << 32);
      EXPECT_THAT(prctl(PR_SET_SECUREBITS, high_bit | SECBIT_KEEP_CAPS, 0, 0, 0),
                  SyscallFailsWithErrno(EPERM));
      EXPECT_THAT(prctl(PR_GET_SECUREBITS), SyscallSucceedsWithValue(0));
    }

    // Extra arguments to PR_SET_SECUREBITS and PR_GET_SECUREBITS are ignored.
    EXPECT_THAT(prctl(PR_SET_SECUREBITS, SECBIT_KEEP_CAPS, 1, 2, 3), SyscallSucceeds());
    EXPECT_THAT(prctl(PR_GET_SECUREBITS, 1, 2, 3, 4), SyscallSucceedsWithValue(SECBIT_KEEP_CAPS));

    // Without CAP_SETPCAP, PR_SET_SECUREBITS fails with EPERM even when setting to the
    // current value or toggling SECBIT_KEEP_CAPS.
    test_helper::UnsetCapabilityEffective(CAP_SETPCAP);
    EXPECT_THAT(prctl(PR_SET_SECUREBITS, SECBIT_KEEP_CAPS, 0, 0, 0), SyscallFailsWithErrno(EPERM));
    EXPECT_THAT(prctl(PR_SET_SECUREBITS, 0, 0, 0, 0), SyscallFailsWithErrno(EPERM));
    EXPECT_THAT(prctl(PR_SET_SECUREBITS, SECBIT_NOROOT, 0, 0, 0), SyscallFailsWithErrno(EPERM));

    // PR_GET_SECUREBITS and PR_SET_KEEPCAPS still succeed without CAP_SETPCAP.
    EXPECT_THAT(prctl(PR_GET_SECUREBITS), SyscallSucceedsWithValue(SECBIT_KEEP_CAPS));
    EXPECT_THAT(prctl(PR_SET_KEEPCAPS, 0), SyscallSucceeds());
    EXPECT_THAT(prctl(PR_GET_SECUREBITS), SyscallSucceedsWithValue(0));
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(PrctlTest, PDeathSigGetSetAndFork) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    int pdeathsig = -1;
    // PR_GET_PDEATHSIG initially returns 0.
    ASSERT_THAT(prctl(PR_GET_PDEATHSIG, &pdeathsig), SyscallSucceeds());
    EXPECT_EQ(pdeathsig, 0);

    // Setting an invalid signal fails with EINVAL.
    EXPECT_THAT(prctl(PR_SET_PDEATHSIG, -1), SyscallFailsWithErrno(EINVAL));
    EXPECT_THAT(prctl(PR_SET_PDEATHSIG, 1024), SyscallFailsWithErrno(EINVAL));

    // Set a valid parent death signal (SIGUSR1).
    ASSERT_THAT(prctl(PR_SET_PDEATHSIG, SIGUSR1), SyscallSucceeds());
    ASSERT_THAT(prctl(PR_GET_PDEATHSIG, &pdeathsig), SyscallSucceeds());
    EXPECT_EQ(pdeathsig, SIGUSR1);

    // Newly created child process does not inherit the parent death signal.
    test_helper::ForkHelper child_helper;
    child_helper.RunInForkedProcess([] {
      int child_pdeathsig = -1;
      ASSERT_THAT(prctl(PR_GET_PDEATHSIG, &child_pdeathsig), SyscallSucceeds());
      EXPECT_EQ(child_pdeathsig, 0);
    });
    EXPECT_TRUE(child_helper.WaitForChildren());

    // Setting 0 clears the parent death signal.
    ASSERT_THAT(prctl(PR_SET_PDEATHSIG, 0), SyscallSucceeds());
    ASSERT_THAT(prctl(PR_GET_PDEATHSIG, &pdeathsig), SyscallSucceeds());
    EXPECT_EQ(pdeathsig, 0);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(PrctlTest, PDeathSigOnParentAndSubreaperExit) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    auto child_ready = test_helper::MakeRendezvous();
    auto parent_signal_received = test_helper::MakeRendezvous();

    helper.RunInForkedProcess([&] {
      // ForkHelper's constructor enables PR_SET_CHILD_SUBREAPER on this process.
      // Only wait for the direct child (`parent`) so that the reparented grandchild remains
      // alive when this subreaper process exits.
      test_helper::ForkHelper subreaper_helper;
      subreaper_helper.OnlyWaitForForkedChildren();
      pid_t subreaper_pid = getpid();

      subreaper_helper.RunInForkedProcess([&] {
        pid_t current_parent_pid = getpid();
        helper.RunInForkedProcess([&] {
          test_helper::SignalMaskHelper sigmask;
          sigmask.blockSignal(SIGUSR1);
          ASSERT_THAT(prctl(PR_SET_PDEATHSIG, SIGUSR1), SyscallSucceeds());

          // Signal the parent process that PR_SET_PDEATHSIG is configured.
          child_ready.poker.poke();

          // Expect SIGUSR1 from the original parent process when it exits.
          siginfo_t info = {};
          EXPECT_EQ(sigmask.timedWaitForSignal(SIGUSR1, 5000, &info), SIGUSR1);
          EXPECT_EQ(info.si_pid, current_parent_pid);

          // Signal the subreaper process to exit and verify a second SIGUSR1 from the subreaper.
          parent_signal_received.poker.poke();

          info = {};
          EXPECT_EQ(sigmask.timedWaitForSignal(SIGUSR1, 5000, &info), SIGUSR1);
          EXPECT_EQ(info.si_pid, subreaper_pid);
        });

        child_ready.holder.hold();
      });

      parent_signal_received.holder.hold();
      EXPECT_TRUE(subreaper_helper.WaitForChildren());
    });
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(PrctlTest, PDeathSigOnCreatorThreadExit) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    test_helper::ForkHelper thread_fork_helper;
    auto child_ready = test_helper::MakeRendezvous();
    pid_t parent_pid = getpid();

    std::thread creator_thread([&] {
      thread_fork_helper.RunInForkedProcess([&] {
        test_helper::SignalMaskHelper sigmask;
        sigmask.blockSignal(SIGUSR1);
        ASSERT_THAT(prctl(PR_SET_PDEATHSIG, SIGUSR1), SyscallSucceeds());

        child_ready.poker.poke();

        siginfo_t info = {};
        EXPECT_EQ(sigmask.timedWaitForSignal(SIGUSR1, 5000, &info), SIGUSR1);
        EXPECT_EQ(info.si_pid, parent_pid);
      });
      child_ready.holder.hold();
    });
    creator_thread.join();

    EXPECT_TRUE(thread_fork_helper.WaitForChildren());
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(PrctlTest, PDeathSigClearedOnCredentialChange) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }

  constexpr uid_t kUser1Uid = 65533;
  constexpr gid_t kUser1Gid = 65534;

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    ASSERT_THAT(prctl(PR_SET_PDEATHSIG, SIGUSR1), SyscallSucceeds());

    SAFE_SYSCALL(setgroups(0, nullptr));
    SAFE_SYSCALL(setresgid(kUser1Gid, kUser1Gid, kUser1Gid));
    SAFE_SYSCALL(setresuid(kUser1Uid, kUser1Uid, kUser1Uid));

    int pdeathsig = -1;
    ASSERT_THAT(prctl(PR_GET_PDEATHSIG, &pdeathsig), SyscallSucceeds());
    EXPECT_EQ(pdeathsig, 0);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(PrctlTest, PDeathSigExec) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }

  std::string helper_binary = test_helper::GetTestResourcePath("print_helper");
  test_helper::ScopedTempDir temp_dir;
  auto mount = ASSERT_RESULT_SUCCESS_AND_RETURN(
      test_helper::ScopedMount::Mount("none", temp_dir.path(), "tmpfs"));

  constexpr int kDirPerms = S_IRWXU | S_IXGRP | S_IXOTH;
  SAFE_SYSCALL(chmod(temp_dir.path().c_str(), kDirPerms));

  std::string normal_binary = temp_dir.path() + "/normal_print_helper";
  std::string suid_binary = temp_dir.path() + "/suid_print_helper";
  std::filesystem::copy_file(helper_binary, normal_binary);
  std::filesystem::copy_file(helper_binary, suid_binary);

  constexpr uid_t kRootUid = 0;
  constexpr gid_t kRootGid = 0;
  constexpr uid_t kUser1Uid = 65533;
  constexpr gid_t kUser1Gid = 65534;

  SAFE_SYSCALL(chown(normal_binary.c_str(), kRootUid, kRootGid));
  SAFE_SYSCALL(chmod(normal_binary.c_str(), S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH));

  SAFE_SYSCALL(chown(suid_binary.c_str(), kRootUid, kRootGid));
  SAFE_SYSCALL(
      chmod(suid_binary.c_str(), S_ISUID | S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH));

  auto run_exec_and_read_pdeathsig = [&](const std::string &binary_path) -> int {
    fbl::unique_fd out_fd(SAFE_SYSCALL(test_helper::MemFdCreate("pdeathsig_out", O_RDWR)));
    test_helper::ForkHelper helper;
    helper.RunInForkedProcess([&] {
      SAFE_SYSCALL(dup2(out_fd.get(), STDOUT_FILENO));
      SAFE_SYSCALL(setgroups(0, nullptr));
      SAFE_SYSCALL(setresgid(kUser1Gid, kUser1Gid, kUser1Gid));
      SAFE_SYSCALL(setresuid(kUser1Uid, kUser1Uid, kUser1Uid));
      SAFE_SYSCALL(prctl(PR_SET_PDEATHSIG, SIGUSR1));

      char *const argv[] = {
          const_cast<char *>(binary_path.c_str()),
          const_cast<char *>("pdeathsig"),
          nullptr,
      };
      char *const envp[] = {nullptr};
      SAFE_SYSCALL(execve(binary_path.c_str(), argv, envp));
    });
    EXPECT_TRUE(helper.WaitForChildren());

    SAFE_SYSCALL(lseek(out_fd.get(), 0, SEEK_SET));
    char buf[32] = {};
    ssize_t bytes_read = SAFE_SYSCALL(read(out_fd.get(), buf, sizeof(buf) - 1));
    EXPECT_GT(bytes_read, 0);
    char *end = nullptr;
    int observed_pdeathsig = static_cast<int>(strtol(buf, &end, 10));
    EXPECT_EQ(*end, '\n');
    return observed_pdeathsig;
  };

  // Non-secure execve(2) preserves the parent death signal.
  EXPECT_EQ(run_exec_and_read_pdeathsig(normal_binary), SIGUSR1);

  // Set-user-ID execve(2) clears the parent death signal.
  EXPECT_EQ(run_exec_and_read_pdeathsig(suid_binary), 0);
}

#if defined(__x86_64__)

TEST(ArchPrctlTest, GetSetFs) {
  constexpr uintptr_t kNonCanonicalFsBase = 0x4141414142424242;
  constexpr uintptr_t kCustomFsBase = 0x12345678;

  uintptr_t orig_fs = 0;
  ASSERT_THAT(syscall(SYS_arch_prctl, ARCH_GET_FS, &orig_fs), SyscallSucceeds());
  EXPECT_NE(orig_fs, 0u);

  // On x86_64 ELF ABI, %fs:0 holds the thread pointer itself.
  uintptr_t tls_self = 0;
  __asm__ volatile("movq %%fs:0, %0" : "=r"(tls_self));
  EXPECT_EQ(orig_fs, tls_self);

  // Temporarily set FS to a custom value, read it back with ARCH_GET_FS, and restore orig_fs
  // inside a single asm block so the compiler cannot emit %fs-relative loads in between.
  uintptr_t read_custom_fs = 0;
  int64_t set_res = -1;
  int64_t get_res = -1;
  int64_t restore_res = -1;
  __asm__ volatile(
      "movq %[sys_nr], %%rax\n\t"
      "movq %[set_fs], %%rdi\n\t"
      "movq %[custom_fs], %%rsi\n\t"
      "syscall\n\t"
      "movq %%rax, %[set_res]\n\t"
      "movq %[sys_nr], %%rax\n\t"
      "movq %[get_fs], %%rdi\n\t"
      "movq %[read_ptr], %%rsi\n\t"
      "syscall\n\t"
      "movq %%rax, %[get_res]\n\t"
      "movq %[sys_nr], %%rax\n\t"
      "movq %[set_fs], %%rdi\n\t"
      "movq %[orig_fs], %%rsi\n\t"
      "syscall\n\t"
      : [set_res] "=&r"(set_res), [get_res] "=&r"(get_res), [restore_res] "=&a"(restore_res)
      : [sys_nr] "i"(SYS_arch_prctl), [set_fs] "i"(ARCH_SET_FS), [get_fs] "i"(ARCH_GET_FS),
        [custom_fs] "r"(kCustomFsBase), [read_ptr] "r"(&read_custom_fs), [orig_fs] "r"(orig_fs)
      : "rdi", "rsi", "rcx", "r11", "memory");
  EXPECT_EQ(set_res, 0);
  EXPECT_EQ(get_res, 0);
  EXPECT_EQ(restore_res, 0);
  EXPECT_EQ(read_custom_fs, kCustomFsBase);

  // Setting FS to a non-canonical address should fail with EPERM and leave FS unchanged.
  EXPECT_THAT(syscall(SYS_arch_prctl, ARCH_SET_FS, kNonCanonicalFsBase),
              SyscallFailsWithErrno(EPERM));

  uintptr_t current_fs = 0;
  ASSERT_THAT(syscall(SYS_arch_prctl, ARCH_GET_FS, &current_fs), SyscallSucceeds());
  EXPECT_EQ(current_fs, orig_fs);

  // Passing an invalid user pointer to ARCH_GET_FS should fail with EFAULT.
  EXPECT_THAT(syscall(SYS_arch_prctl, ARCH_GET_FS, nullptr), SyscallFailsWithErrno(EFAULT));

  // Verify that a newly spawned thread has a distinct non-zero FS base.
  uintptr_t thread_fs = 0;
  std::thread worker([&thread_fs] {
    EXPECT_THAT(syscall(SYS_arch_prctl, ARCH_GET_FS, &thread_fs), SyscallSucceeds());
  });
  worker.join();
  EXPECT_NE(thread_fs, 0u);
  EXPECT_NE(thread_fs, orig_fs);
}

TEST(ArchPrctlTest, GetSetGs) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    constexpr uintptr_t kNonCanonicalGsBase = 0x4141414142424242;
    constexpr uint64_t kMagicValue = 0xdeadbeefcafebabeULL;

    uintptr_t orig_gs = 0;
    ASSERT_THAT(syscall(SYS_arch_prctl, ARCH_GET_GS, &orig_gs), SyscallSucceeds());

    auto mapping = ASSERT_RESULT_SUCCESS_AND_RETURN(test_helper::ScopedMMap::MMap(
        nullptr, getpagesize(), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    auto *data = static_cast<uint64_t *>(mapping.mapping());
    *data = kMagicValue;

    uintptr_t new_gs = reinterpret_cast<uintptr_t>(mapping.mapping());
    ASSERT_THAT(syscall(SYS_arch_prctl, ARCH_SET_GS, new_gs), SyscallSucceeds());

    uintptr_t read_gs = 0;
    ASSERT_THAT(syscall(SYS_arch_prctl, ARCH_GET_GS, &read_gs), SyscallSucceeds());
    EXPECT_EQ(read_gs, new_gs);

    uint64_t gs_val = 0;
    __asm__ volatile("movq %%gs:0, %0" : "=r"(gs_val));
    EXPECT_EQ(gs_val, kMagicValue);

    // Setting GS to a non-canonical address should fail with EPERM and leave GS unchanged.
    EXPECT_THAT(syscall(SYS_arch_prctl, ARCH_SET_GS, kNonCanonicalGsBase),
                SyscallFailsWithErrno(EPERM));

    read_gs = 0;
    ASSERT_THAT(syscall(SYS_arch_prctl, ARCH_GET_GS, &read_gs), SyscallSucceeds());
    EXPECT_EQ(read_gs, new_gs);

    // Passing an invalid user pointer to ARCH_GET_GS should fail with EFAULT.
    EXPECT_THAT(syscall(SYS_arch_prctl, ARCH_GET_GS, nullptr), SyscallFailsWithErrno(EFAULT));

    // Restore original GS base.
    ASSERT_THAT(syscall(SYS_arch_prctl, ARCH_SET_GS, orig_gs), SyscallSucceeds());
    ASSERT_THAT(syscall(SYS_arch_prctl, ARCH_GET_GS, &read_gs), SyscallSucceeds());
    EXPECT_EQ(read_gs, orig_gs);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

#endif  // defined(__x86_64__)

}  // namespace
