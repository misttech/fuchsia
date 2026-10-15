// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include <endian.h>
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <lib/fit/defer.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <filesystem>
#include <functional>
#include <string>
#include <vector>

#include <fbl/unique_fd.h>
#include <gtest/gtest-spi.h>
#include <gtest/gtest.h>
#include <linux/capability.h>
#include <linux/prctl.h>
#include <linux/securebits.h>

#include "src/lib/files/file.h"
#include "src/lib/fxl/strings/string_printf.h"
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

struct capability_t {
  int cap_num;
  int effective;
  int permitted;
  int inheritable;
  int bounding;
  int ambient;
};

static char kPrintHelperBinary[] = "print_helper";

constexpr uid_t kRootUid = 0;
constexpr gid_t kRootGid = 0;
constexpr uid_t kUser1Uid = 65533;
constexpr gid_t kUser1Gid = 65534;
constexpr uid_t kUser2Uid = 65531;
constexpr gid_t kUser2Gid = 65532;

void SwitchToUser1(bool keep_caps = false) {
  ASSERT_THAT(setresgid(kUser1Gid, kUser1Gid, kUser1Gid), SyscallSucceeds());
  ASSERT_THAT(setgroups(0, nullptr), SyscallSucceeds());
  if (keep_caps) {
    ASSERT_THAT(prctl(PR_SET_SECUREBITS, SECBIT_KEEP_CAPS), SyscallSucceeds());
  }
  ASSERT_THAT(setresuid(kUser1Uid, kUser1Uid, kUser1Uid), SyscallSucceeds());
}

// Runs a program with a single argument in a child process, wiring stdout to a
// given file descriptor. Runs the given prelude code inside the child process
// before calling execve.
testing::AssertionResult RunSimpleProgram(std::function<void()> prelude, std::string program_path,
                                          std::string argv1, int stdout_fd = 1) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&]() {
    prelude();
    if (testing::Test::HasFailure()) {
      return;
    }

    if (stdout_fd != 1) {
      ASSERT_THAT(dup2(stdout_fd, 1), SyscallSucceeds());
    }

    char *const argv[] = {const_cast<char *>(program_path.c_str()),
                          const_cast<char *>(argv1.c_str()), nullptr};
    char *const envp[] = {nullptr};

    ASSERT_THAT(execve(program_path.c_str(), argv, envp), SyscallSucceeds());
  });

  return helper.WaitForChildren();
}

// Probes whether user namespaces (CLONE_NEWUSER) are supported in this environment.
// Returns true if supported, false if unsupported (EINVAL or EPERM).
bool ProbeUserNamespaceSupport() {
  test_helper::ForkHelper helper;
  helper.OnlyWaitForForkedChildren();
  pid_t pid = helper.RunInForkedProcess([]() {
    ASSERT_THAT(setresgid(9999, 9999, 9999), SyscallSucceeds());
    ASSERT_THAT(setresuid(9999, 9999, 9999), SyscallSucceeds());
    if (unshare(CLONE_NEWUSER) != 0) {
      if (errno == EINVAL || errno == EPERM) {
        _exit(errno);
      }
      FAIL() << "unshare(CLONE_NEWUSER) failed unexpectedly: " << strerror(errno);
    }
  });

  auto result = helper.WaitForChild(pid);
  if (result.subprocess_exit_status == EINVAL || result.subprocess_exit_status == EPERM) {
    return false;
  }
  EXPECT_TRUE(result.determined_result);
  return result.determined_result;
}

class CapsExecTest : public ::testing::Test {
 protected:
  void SetUp() override {
    if (!test_helper::HasSysAdmin()) {
      GTEST_SKIP() << "Not running with sysadmin capabilities. skipping.";
    }

    // The securebits flag should have no bits set. Individual tests can enable
    // what they use.
    ASSERT_EQ(SAFE_SYSCALL(prctl(PR_GET_SECUREBITS)), 0);

    // The test should be able to control the mount flags.
    mount_ = ASSERT_RESULT_SUCCESS_AND_RETURN(
        test_helper::ScopedMount::Mount("", temp_dir_.path(), "tmpfs", 0, nullptr));

    constexpr int kDirPerms = S_IRWXU | S_IXGRP | S_IXOTH;
    SAFE_SYSCALL(chmod(temp_dir_.path().c_str(), kDirPerms));

    // Copy out the test binary into the temporary directory.
    std::filesystem::path print_helper_binary =
        test_helper::GetTestResourcePath(kPrintHelperBinary);

    print_helper_ = std::filesystem::path(temp_dir_.path()) / kPrintHelperBinary;
    std::error_code ec;
    std::filesystem::copy_file(print_helper_binary.c_str(), print_helper_.c_str(), ec);
    ASSERT_FALSE(ec) << "failed to copy file: " << ec;

    SAFE_SYSCALL(chown(print_helper_.c_str(), kUser1Uid, kUser1Gid));
    SAFE_SYSCALL(chmod(print_helper_.c_str(), S_IRWXU | S_IXGRP | S_IXOTH));

    // The file has no capabilities.
    ASSERT_EQ(getxattr(print_helper_.c_str(), "security.capability", nullptr, 0), -1);
    ASSERT_EQ(errno, ENODATA);

    FILE *fp = fopen("/proc/sys/kernel/cap_last_cap", "r");
    ASSERT_NE(fp, nullptr);
    int n = fscanf(fp, "%d\n", &cap_last_cap_);
    ASSERT_EQ(n, 1);
    fclose(fp);
  }

  std::filesystem::path path() const { return temp_dir_.path(); }

  testing::AssertionResult RunPrintSecurebits(std::function<void()> prelude,
                                              unsigned int *securebits) {
    fbl::unique_fd stdout_fd(SAFE_SYSCALL(test_helper::MemFdCreate("output", 0)));

    testing::AssertionResult result =
        RunSimpleProgram(prelude, print_helper_, "securebits", stdout_fd.get());

    if (!result) {
      return result;
    }

    SAFE_SYSCALL(lseek(stdout_fd.get(), 0, SEEK_SET));
    FILE *fp = fdopen(stdout_fd.release(), "r");
    if (fp == nullptr) {
      return ::testing::AssertionFailure() << "failed to open output fd\n" << strerror(errno);
    }
    auto cleanup = fit::defer([fp]() { SAFE_SYSCALL(fclose(fp)); });

    if (fscanf(fp, "%x\n", securebits) != 1) {
      return ::testing::AssertionFailure() << "failed to read securebits\n";
    }
    return ::testing::AssertionSuccess();
  }

  testing::AssertionResult RunPrintAtSecure(std::function<void()> prelude,
                                            unsigned long *at_secure) {
    fbl::unique_fd stdout_fd(SAFE_SYSCALL(test_helper::MemFdCreate("output", 0)));

    testing::AssertionResult result =
        RunSimpleProgram(prelude, print_helper_, "at_secure", stdout_fd.get());

    if (!result) {
      return result;
    }

    SAFE_SYSCALL(lseek(stdout_fd.get(), 0, SEEK_SET));
    FILE *fp = fdopen(stdout_fd.release(), "r");
    if (fp == nullptr) {
      return ::testing::AssertionFailure() << "failed to open output fd\n" << strerror(errno);
    }
    auto cleanup = fit::defer([fp]() { SAFE_SYSCALL(fclose(fp)); });

    if (fscanf(fp, "%lu\n", at_secure) != 1) {
      return ::testing::AssertionFailure() << "failed to read at_secure\n";
    }
    return ::testing::AssertionSuccess();
  }

  testing::AssertionResult RunPrintUids(std::function<void()> prelude, uid_t *ruid, uid_t *euid,
                                        uid_t *suid) {
    fbl::unique_fd stdout_fd(SAFE_SYSCALL(test_helper::MemFdCreate("output", 0)));

    testing::AssertionResult result =
        RunSimpleProgram(prelude, print_helper_, "uids", stdout_fd.get());

    if (!result) {
      return result;
    }

    SAFE_SYSCALL(lseek(stdout_fd.get(), 0, SEEK_SET));
    FILE *fp = fdopen(stdout_fd.release(), "r");
    if (fp == nullptr) {
      return ::testing::AssertionFailure() << "failed to open output fd\n" << strerror(errno);
    }
    auto cleanup = fit::defer([fp]() { SAFE_SYSCALL(fclose(fp)); });

    if (fscanf(fp, "%u,%u,%u\n", ruid, euid, suid) != 3) {
      return ::testing::AssertionFailure() << "failed to read uids\n";
    }
    return ::testing::AssertionSuccess();
  }

  void RunExecExpectError(std::function<void()> prelude, int expected_errno) {
    test_helper::ForkHelper helper;
    helper.RunInForkedProcess([&]() {
      prelude();
      if (testing::Test::HasFailure()) {
        return;
      }

      char *const argv[] = {const_cast<char *>(print_helper_.c_str()),
                            const_cast<char *>("unexpected_execve_success"), nullptr};
      char *const envp[] = {nullptr};

      EXPECT_THAT(execve(print_helper_.c_str(), argv, envp), SyscallFailsWithErrno(expected_errno));
    });
    EXPECT_TRUE(helper.WaitForChildren());
  }

  testing::AssertionResult RunPrintCapabilities(std::function<void()> prelude,
                                                std::vector<capability_t> &capabilities) {
    capabilities.clear();
    ::testing::AssertionResult result = ::testing::AssertionSuccess();
    fbl::unique_fd stdout_fd(SAFE_SYSCALL(test_helper::MemFdCreate("output", 0)));

    result = RunSimpleProgram(prelude, print_helper_, "capabilities", stdout_fd.get());
    if (!result) {
      return result;
    }

    SAFE_SYSCALL(lseek(stdout_fd.get(), 0, SEEK_SET));
    FILE *fp = fdopen(stdout_fd.release(), "r");
    if (fp == nullptr) {
      return ::testing::AssertionFailure() << "failed to open output fd\n" << strerror(errno);
    }
    auto cleanup = fit::defer([fp]() { SAFE_SYSCALL(fclose(fp)); });

    char buf[100] = {0};
    int n = fscanf(fp, "%99s\n", &buf[0]);
    if (n != 1) {
      return ::testing::AssertionFailure() << "failed to read result header";
    }

    if (strcmp(buf, "CAP_NUM,EFFECTIVE,PERMITTED,INHERITABLE,BOUNDING,AMBIENT") != 0) {
      return ::testing::AssertionFailure() << "Header doesn't match expected value. Got: " << buf;
    }

    for (int expected_cap = 0; expected_cap <= cap_last_cap_; expected_cap++) {
      capability_t capability{};
      int n = fscanf(fp, "%d,%d,%d,%d,%d,%d\n", &capability.cap_num, &capability.effective,
                     &capability.permitted, &capability.inheritable, &capability.bounding,
                     &capability.ambient);
      if (n != 6) {
        return ::testing::AssertionFailure() << "invalid row";
      }

      if (capability.cap_num != expected_cap) {
        return ::testing::AssertionFailure() << "Unexpected capability number";
      }

      auto is_valid = [](int val) { return (val == 1 || val == 0); };
      if (!is_valid(capability.effective) || !is_valid(capability.permitted) ||
          !is_valid(capability.inheritable) || !is_valid(capability.bounding) ||
          !is_valid(capability.ambient)) {
        return ::testing::AssertionFailure() << "Invalid capability value";
      }
      capabilities.push_back(capability);
    }

    return result;
  }

  static void AttachTracerToCurrentProcess() {
    ASSERT_THAT(prctl(PR_SET_DUMPABLE, 1), SyscallSucceeds());
#ifdef PR_SET_PTRACER
    prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY);
#endif

    test_helper::Rendezvous ready = test_helper::MakeRendezvous();

    pid_t tracee_pid = getpid();
    pid_t tracer_pid = SAFE_SYSCALL(fork());
    if (tracer_pid == 0) {
      ready.holder = test_helper::Holder();
      ASSERT_THAT(ptrace(PTRACE_ATTACH, tracee_pid, nullptr, nullptr), SyscallSucceeds());
      int status = 0;
      ASSERT_THAT(waitpid(tracee_pid, &status, 0), SyscallSucceedsWithValue(tracee_pid));
      ASSERT_TRUE(WIFSTOPPED(status));
      ASSERT_THAT(ptrace(PTRACE_CONT, tracee_pid, nullptr, nullptr), SyscallSucceeds());
      ready.poker.poke();

      ASSERT_THAT(waitpid(tracee_pid, &status, 0), SyscallSucceedsWithValue(tracee_pid));
      if (WIFSTOPPED(status)) {
        ASSERT_THAT(ptrace(PTRACE_DETACH, tracee_pid, nullptr, nullptr), SyscallSucceeds());
      }
      fflush(nullptr);
      _exit(testing::Test::HasFailure());
    }

    ready.poker = test_helper::Poker();
    ready.holder.hold();
  }

  test_helper::ScopedTempDir temp_dir_;
  test_helper::ScopedMount mount_;
  std::filesystem::path print_helper_;
  int cap_last_cap_;
};

// Helper to set V2 file capabilities (20 bytes).
// - permitted: 64-bit mask of permitted capabilities
// - inheritable: 64-bit mask of inheritable capabilities
// - effective: whether to set the effective bit in the magic
// Returns 0 on success, or -1 on error (with errno set).
int SetFileCapabilitiesV2(const std::filesystem::path &path, uint64_t permitted,
                          uint64_t inheritable, bool effective) {
  uint32_t magic = VFS_CAP_REVISION_2;
  if (effective) {
    magic |= VFS_CAP_FLAGS_EFFECTIVE;
  }

  vfs_cap_data cap_data{};
  cap_data.magic_etc = htole32(magic);
  cap_data.data[0].permitted = htole32(static_cast<uint32_t>(permitted));
  cap_data.data[0].inheritable = htole32(static_cast<uint32_t>(inheritable));
  cap_data.data[1].permitted = htole32(static_cast<uint32_t>(permitted >> 32u));
  cap_data.data[1].inheritable = htole32(static_cast<uint32_t>(inheritable >> 32u));
  return setxattr(path.c_str(), "security.capability", &cap_data, sizeof(cap_data), 0);
}

// Helper to set V3 file capabilities (24 bytes).
// - permitted: 64-bit mask of permitted capabilities
// - inheritable: 64-bit mask of inheritable capabilities
// - effective: whether to set the effective bit in the magic
// - rootid: the root UID of the user namespace
// Returns 0 on success, or -1 on error (with errno set).
int SetFileCapabilitiesV3(const std::filesystem::path &path, uint64_t permitted,
                          uint64_t inheritable, bool effective, uid_t rootid) {
  uint32_t magic = VFS_CAP_REVISION_3;
  if (effective) {
    magic |= VFS_CAP_FLAGS_EFFECTIVE;
  }

  vfs_ns_cap_data cap_data{};
  cap_data.magic_etc = htole32(magic);
  cap_data.data[0].permitted = htole32(static_cast<uint32_t>(permitted));
  cap_data.data[0].inheritable = htole32(static_cast<uint32_t>(inheritable));
  cap_data.data[1].permitted = htole32(static_cast<uint32_t>(permitted >> 32u));
  cap_data.data[1].inheritable = htole32(static_cast<uint32_t>(inheritable >> 32u));
  cap_data.rootid = htole32(static_cast<uint32_t>(rootid));
  return setxattr(path.c_str(), "security.capability", &cap_data, sizeof(cap_data), 0);
}

}  // namespace

TEST_F(CapsExecTest, SecurebitKeepCapsIsNotPreservedAcrossExecve) {
  unsigned int securebits;

  ASSERT_TRUE(RunPrintSecurebits(
      []() { SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, SECBIT_KEEP_CAPS | SECBIT_KEEP_CAPS_LOCKED)); },
      &securebits));

  EXPECT_NE(securebits & SECBIT_KEEP_CAPS, static_cast<unsigned int>(SECBIT_KEEP_CAPS));
  EXPECT_EQ(securebits & SECBIT_KEEP_CAPS_LOCKED,
            static_cast<unsigned int>(SECBIT_KEEP_CAPS_LOCKED));
}

TEST_F(CapsExecTest, SecurebitFlagsArePreservedAcrossExecve) {
  unsigned int securebits;

  constexpr unsigned int kSecurebitInheritableFlags =
      SECBIT_NO_SETUID_FIXUP | SECBIT_NO_SETUID_FIXUP_LOCKED | SECBIT_NOROOT |
      SECBIT_NOROOT_LOCKED | SECBIT_NO_CAP_AMBIENT_RAISE | SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED;
  ASSERT_TRUE(RunPrintSecurebits(
      []() { SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, kSecurebitInheritableFlags)); }, &securebits));

  EXPECT_EQ(securebits & kSecurebitInheritableFlags, kSecurebitInheritableFlags);
}

TEST_F(CapsExecTest, NonRootExecutingRegularBinaryClearsPermittedAndEffectiveCaps) {
  // If a non-root user executes a regular binary, without file capabilities,
  // it should clear its permitted and effective capabilities.
  std::vector<capability_t> capabilities;

  ASSERT_TRUE(RunPrintCapabilities(
      []() {
        // We want to keep capabilities after switching uids.
        SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, SECBIT_KEEP_CAPS));

        // After switching to a non-root uid, with the secbits
        // all our effective capabilities are cleaned, but the permitted are
        // still there.
        SAFE_SYSCALL(setresuid(kUser1Uid, kUser1Uid, kUser1Gid));

        // CAP_AUDIT_READ will be effective and permitted.
        test_helper::SetCapabilityEffective(CAP_AUDIT_READ);

        // CAP_AUDIT_WRITE will be effective, permitted, and inheritable.
        test_helper::SetCapabilityEffective(CAP_AUDIT_WRITE);
        test_helper::SetCapabilityInheritable(CAP_AUDIT_WRITE);
      },
      capabilities));

  for (const auto &capability : capabilities) {
    EXPECT_EQ(capability.effective, 0);
    EXPECT_EQ(capability.permitted, 0);
  }
}

TEST_F(CapsExecTest, RootExecutingRegularBinaryGetsAllCapabilitiesBack) {
  // A program with euid root will get all capabilities back when it executes a
  // regular binary.
  std::vector<capability_t> capabilities;

  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        // This test doesn't care about CAP_SYS_ADMIN, it only cares about
        // having the root uid.
        SAFE_SYSCALL(setresuid(kRootUid, kRootUid, kRootUid));

        // We expect to have all bounding capabilities set.
        for (int cap = 0; cap <= cap_last_cap_; cap++) {
          ASSERT_TRUE(test_helper::HasCapabilityBounding(cap));
        }

        // Disable one: this should not be enabled after execve.
        test_helper::UnsetCapabilityBounding(CAP_AUDIT_READ);

        test_helper::DropAllCapabilities();
        test_helper::DropAllAmbientCapabilities();
      },
      capabilities));

  for (int cap = 0; cap <= cap_last_cap_; cap++) {
    if (cap == CAP_AUDIT_READ) {
      continue;
    }

    EXPECT_EQ(capabilities[cap].effective, 1);
    EXPECT_EQ(capabilities[cap].permitted, 1);
    EXPECT_EQ(capabilities[cap].inheritable, 0);
    EXPECT_EQ(capabilities[cap].bounding, 1);
    EXPECT_EQ(capabilities[cap].ambient, 0);
  }

  // The one we removed from the bounding set was not added.
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].effective, 0);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].permitted, 0);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].inheritable, 0);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].bounding, 0);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].ambient, 0);
}

TEST_F(CapsExecTest, RootExecutingRegularBinaryWithNoNewPrivsDoesNotGetCapabilitiesBack) {
  std::vector<capability_t> capabilities;

  ASSERT_TRUE(RunPrintCapabilities(
      []() {
        // Ensure that the sub-process is executing as the root user.
        SAFE_SYSCALL(setresuid(kRootUid, kRootUid, kRootUid));

        // Set the no-new-privileges bit for the process.
        SAFE_SYSCALL(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0));

        // Drop the `CAP_AUDIT_READ` capability from the effective & permitted sets.
        test_helper::UnsetCapabilityEffective(CAP_AUDIT_READ);
        test_helper::UnsetCapabilityPermitted(CAP_AUDIT_READ);
      },
      capabilities));

  EXPECT_EQ(capabilities[CAP_AUDIT_READ].effective, 0);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].permitted, 0);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].inheritable, 0);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].bounding, 1);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].ambient, 0);
}

TEST_F(CapsExecTest, ChangingFromRootUidDropsAllCapabilities) {
  test_helper::ForkHelper helper;

  ASSERT_EQ(geteuid(), kRootUid);
  helper.RunInForkedProcess([&]() {
    // Effective capabilities are cleared when the effective UID transitions from root
    // to non-root, so the effective UID before setting up the capabilities.
    SAFE_SYSCALL(setresuid(kUser1Uid, kUser1Uid, 0));

    // Set CAP_SYSLOG effective, inheritable and ambient, so we can check that the
    // ambient and effective sets are cleared.
    test_helper::SetCapabilityEffective(CAP_SYSLOG);
    test_helper::SetCapabilityInheritable(CAP_SYSLOG);
    test_helper::SetCapabilityAmbient(CAP_SYSLOG);
    ASSERT_TRUE(test_helper::HasCapabilityAmbient(CAP_SYSLOG));
    ASSERT_TRUE(test_helper::HasCapabilityEffective(CAP_SYSLOG));

    // Changing from a root uid to a non-root uid drops all capabilities.
    SAFE_SYSCALL(setresuid(kUser1Uid, kUser1Uid, kUser1Uid));

    for (int cap = 0; cap <= cap_last_cap_; cap++) {
      EXPECT_TRUE(test_helper::HasCapabilityBounding(cap));
      EXPECT_FALSE(test_helper::HasCapabilityAmbient(cap));
      EXPECT_FALSE(test_helper::HasCapabilityEffective(cap));
      EXPECT_FALSE(test_helper::HasCapabilityPermitted(cap));
      EXPECT_EQ(test_helper::HasCapabilityInheritable(cap), cap == CAP_SYSLOG);
    }
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST_F(CapsExecTest, ChangingFromRootUidWithKeepCapsDropsAllEffectiveCapabilities) {
  test_helper::ForkHelper helper;

  ASSERT_EQ(geteuid(), kRootUid);
  helper.RunInForkedProcess([&]() {
    // Effective capabilities are cleared when the effective UID transitions from root
    // to non-root, so the effective UID before setting up the capabilities.
    SAFE_SYSCALL(setresuid(kUser1Uid, kUser1Uid, 0));

    // Set CAP_SYSLOG effective, inheritable and ambient, so we can check that the
    // ambient bit is cleared even if SECBITS_KEEP_CAPS is set.
    test_helper::SetCapabilityEffective(CAP_SYSLOG);
    test_helper::SetCapabilityInheritable(CAP_SYSLOG);
    test_helper::SetCapabilityAmbient(CAP_SYSLOG);
    ASSERT_TRUE(test_helper::HasCapabilityAmbient(CAP_SYSLOG));
    ASSERT_TRUE(test_helper::HasCapabilityEffective(CAP_SYSLOG));

    // Required in order to be able to set `SECBIT_KEEP_CAPS`.
    test_helper::SetCapabilityEffective(CAP_SETPCAP);
    SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, SECBIT_KEEP_CAPS));
    test_helper::UnsetCapabilityEffective(CAP_SETPCAP);

    // With SECBIT_KEEP_CAPS, changing from a root euid to a non-root euid drops all effective
    // capabilities.
    SAFE_SYSCALL(setresuid(kUser1Uid, kUser1Uid, kUser1Uid));

    for (int cap = 0; cap <= cap_last_cap_; cap++) {
      EXPECT_TRUE(test_helper::HasCapabilityBounding(cap));
      EXPECT_FALSE(test_helper::HasCapabilityAmbient(cap));
      EXPECT_EQ(test_helper::HasCapabilityEffective(cap), cap == CAP_SYSLOG);
      EXPECT_TRUE(test_helper::HasCapabilityPermitted(cap));
      EXPECT_EQ(test_helper::HasCapabilityInheritable(cap), cap == CAP_SYSLOG);
    }
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST_F(CapsExecTest, ChangingFromRootUidWithNoFixupSetuidKeepsCapabilities) {
  test_helper::ForkHelper helper;

  ASSERT_EQ(geteuid(), kRootUid);
  helper.RunInForkedProcess([&]() {
    SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, SECBIT_NO_SETUID_FIXUP));
    // With SECBIT_NO_SETUID_FIXUP, capabilities don't change when changing uids.
    SAFE_SYSCALL(setresuid(kUser1Uid, kUser1Uid, kUser1Uid));

    for (int cap = 0; cap <= cap_last_cap_; cap++) {
      EXPECT_TRUE(test_helper::HasCapabilityBounding(cap));
      EXPECT_FALSE(test_helper::HasCapabilityAmbient(cap));
      EXPECT_TRUE(test_helper::HasCapabilityEffective(cap));
      EXPECT_TRUE(test_helper::HasCapabilityPermitted(cap));
      EXPECT_FALSE(test_helper::HasCapabilityInheritable(cap));
    }
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST_F(CapsExecTest, RegularUserExecutingSUIDRootGetsAllCapabilities) {
  /*
     When a process with nonzero UIDs execves a set-user-ID root program
     that does not have capabilities attached, the calculation of the permitted
     and effective capabilities is as follows:

     P'(permitted)   = P(inheritable) | P(bounding)
     P'(effective)   = P'(permitted)
  */
  std::vector<capability_t> capabilities;

  SAFE_SYSCALL(chown(print_helper_.c_str(), kRootUid, kRootGid));
  SAFE_SYSCALL(chmod(print_helper_.c_str(), S_ISUID | S_IXOTH | S_IRWXU));

  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        SAFE_SYSCALL(setresgid(kUser1Gid, kUser1Gid, kUser1Gid));
        SAFE_SYSCALL(setgroups(0, nullptr));

        // Will not drop permitted capabilities.
        SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, SECBIT_KEEP_CAPS));
        SAFE_SYSCALL(setresuid(kUser1Uid, kUser1Uid, kUser1Uid));

        test_helper::SetCapabilityEffective(CAP_SETPCAP);

        // Leave only CAP_SETPCAP in effective capabilities.
        for (int cap = 0; cap < cap_last_cap_; cap++) {
          if (cap == CAP_SETPCAP) {
            continue;
          }
          test_helper::UnsetCapabilityEffective(cap);
          test_helper::UnsetCapabilityPermitted(cap);
          test_helper::UnsetCapabilityInheritable(cap);
          test_helper::UnsetCapabilityAmbient(cap);
        }

        // CAP_SYSLOG will be in the bounding set but not in inheritable.
        ASSERT_TRUE(test_helper::HasCapabilityBounding(CAP_SYSLOG));
        ASSERT_FALSE(test_helper::HasCapabilityInheritable(CAP_SYSLOG));

        // CAP_AUDIT_READ will not be in bounding nor in inheritable.
        test_helper::UnsetCapabilityBounding(CAP_AUDIT_READ);

        // CAP_AUDIT_CONTROL will be both inheritable and bounding.
        test_helper::SetCapabilityInheritable(CAP_AUDIT_CONTROL);

        // CAP_AUDIT_WRITE will be inheritable but not in bounding.
        test_helper::SetCapabilityInheritable(CAP_AUDIT_WRITE);
        test_helper::UnsetCapabilityBounding(CAP_AUDIT_WRITE);

        test_helper::UnsetCapabilityEffective(CAP_SETPCAP);
        test_helper::UnsetCapabilityPermitted(CAP_SETPCAP);
        test_helper::UnsetCapabilityInheritable(CAP_SETPCAP);
        test_helper::UnsetCapabilityAmbient(CAP_SETPCAP);
      },
      capabilities));

  EXPECT_EQ(capabilities[CAP_SYSLOG].effective, 1);
  EXPECT_EQ(capabilities[CAP_SYSLOG].permitted, 1);
  EXPECT_EQ(capabilities[CAP_SYSLOG].inheritable, 0);
  EXPECT_EQ(capabilities[CAP_SYSLOG].bounding, 1);
  EXPECT_EQ(capabilities[CAP_SYSLOG].ambient, 0);

  EXPECT_EQ(capabilities[CAP_AUDIT_READ].effective, 0);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].permitted, 0);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].inheritable, 0);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].bounding, 0);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].ambient, 0);

  EXPECT_EQ(capabilities[CAP_AUDIT_WRITE].effective, 1);
  EXPECT_EQ(capabilities[CAP_AUDIT_WRITE].permitted, 1);
  EXPECT_EQ(capabilities[CAP_AUDIT_WRITE].inheritable, 1);
  EXPECT_EQ(capabilities[CAP_AUDIT_WRITE].bounding, 0);
  EXPECT_EQ(capabilities[CAP_AUDIT_WRITE].ambient, 0);

  EXPECT_EQ(capabilities[CAP_AUDIT_CONTROL].effective, 1);
  EXPECT_EQ(capabilities[CAP_AUDIT_CONTROL].permitted, 1);
  EXPECT_EQ(capabilities[CAP_AUDIT_CONTROL].inheritable, 1);
  EXPECT_EQ(capabilities[CAP_AUDIT_CONTROL].bounding, 1);
  EXPECT_EQ(capabilities[CAP_AUDIT_CONTROL].ambient, 0);
}

void SetUpCapabilities(capability_t cap) {
  test_helper::SetCapabilityEffective(CAP_SETPCAP);
  int cap_num = cap.cap_num;

  test_helper::SetCapabilityEffective(cap_num);
  test_helper::SetCapabilityPermitted(cap_num);
  test_helper::SetCapabilityInheritable(cap_num);
  test_helper::SetCapabilityAmbient(cap_num);

  if (cap.effective == 0) {
    test_helper::UnsetCapabilityEffective(cap_num);
  }
  if (cap.permitted == 0) {
    test_helper::UnsetCapabilityPermitted(cap_num);
  }
  if (cap.inheritable == 0) {
    test_helper::UnsetCapabilityInheritable(cap_num);
  }
  if (cap.ambient == 0) {
    test_helper::UnsetCapabilityAmbient(cap_num);
  }
  if (cap.bounding == 0) {
    test_helper::UnsetCapabilityBounding(cap_num);
  }

  EXPECT_EQ(test_helper::HasCapabilityEffective(cap_num), cap.effective);
  EXPECT_EQ(test_helper::HasCapabilityPermitted(cap_num), cap.permitted);
  EXPECT_EQ(test_helper::HasCapabilityInheritable(cap_num), cap.inheritable);
  EXPECT_EQ(test_helper::HasCapabilityAmbient(cap_num), cap.ambient);
  EXPECT_EQ(test_helper::HasCapabilityBounding(cap_num), cap.bounding);
}

TEST(CapsTest, PermittedCapabilitiesCannotBeRegained) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities. skipping.";
  }

  std::vector<capability_t> caps;

  // Capabilities combinations that don't have the permitted bit.
  caps.push_back({CAP_AUDIT_READ, 0, 0, 0, 0, 0});
  caps.push_back({CAP_AUDIT_READ, 0, 0, 0, 1, 0});
  caps.push_back({CAP_AUDIT_READ, 0, 0, 1, 0, 0});
  caps.push_back({CAP_AUDIT_READ, 0, 0, 1, 1, 0});

  test_helper::ForkHelper helper;
  for (const auto &cap : caps) {
    helper.RunInForkedProcess([cap]() {
      SetUpCapabilities(cap);
      EXPECT_NONFATAL_FAILURE(test_helper::SetCapabilityPermitted(cap.cap_num),
                              "Operation not permitted");
    });
    EXPECT_TRUE(helper.WaitForChildren());
  }
}

TEST(CapsTest, PermittedCapabilitiesCannotBeDroppedIfTheyAreEffective) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities. skipping.";
  }

  std::vector<capability_t> caps;

  // Capabilities combinations that have effective and permitted bits.
  caps.push_back({CAP_AUDIT_READ, 1, 1, 0, 0, 0});
  caps.push_back({CAP_AUDIT_READ, 1, 1, 1, 0, 0});
  caps.push_back({CAP_AUDIT_READ, 1, 1, 0, 1, 0});
  caps.push_back({CAP_AUDIT_READ, 1, 1, 1, 1, 0});
  caps.push_back({CAP_AUDIT_READ, 1, 1, 1, 1, 1});

  test_helper::ForkHelper helper;
  for (const auto &cap : caps) {
    helper.RunInForkedProcess([cap]() {
      SetUpCapabilities(cap);
      EXPECT_NONFATAL_FAILURE(test_helper::UnsetCapabilityPermitted(cap.cap_num),
                              "Operation not permitted");
    });
    EXPECT_TRUE(helper.WaitForChildren());
  }
}

TEST(CapsTest, CannotSetEffectiveCapsIfNotPermitted) {
  // Tests that you cannot set a capability as effective if it's not permitted.
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities. skipping.";
  }

  std::vector<capability_t> test_cases;

  test_cases.push_back({CAP_AUDIT_READ, 0, 0, 0, 0, 0});
  test_cases.push_back({CAP_AUDIT_READ, 0, 0, 0, 1, 0});
  test_cases.push_back({CAP_AUDIT_READ, 0, 0, 1, 0, 0});
  test_cases.push_back({CAP_AUDIT_READ, 0, 0, 1, 1, 0});

  test_helper::ForkHelper helper;

  for (const auto &test_case : test_cases) {
    helper.RunInForkedProcess([test_case]() {
      SetUpCapabilities(test_case);
      EXPECT_NONFATAL_FAILURE(test_helper::SetCapabilityEffective(CAP_AUDIT_READ), "SYS_capset");
      EXPECT_FALSE(test_helper::HasCapabilityEffective(CAP_AUDIT_READ));
    });
    EXPECT_TRUE(helper.WaitForChildren());
  }
}

TEST(CapsTest, AmbientCapabilitiesRequirePermittedAndInheritable) {
  // Tests that ambient capabilities will be unset if inheritable or permitted
  // are unset.
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities. skipping.";
  }

  struct test_case_t {
    capability_t caps;
    const char *failure;
  };

  std::vector<test_case_t> test_cases;

  test_cases.push_back({{CAP_AUDIT_READ, 0, 0, 0, 0, 1}, "HasCapabilityAmbient"});
  test_cases.push_back({{CAP_AUDIT_READ, 0, 0, 0, 1, 1}, "HasCapabilityAmbient"});
  test_cases.push_back({{CAP_AUDIT_READ, 0, 0, 1, 0, 1}, "HasCapabilityAmbient"});
  test_cases.push_back({{CAP_AUDIT_READ, 0, 0, 1, 1, 1}, "HasCapabilityAmbient"});
  test_cases.push_back({{CAP_AUDIT_READ, 0, 1, 0, 0, 1}, "HasCapabilityAmbient"});
  test_cases.push_back({{CAP_AUDIT_READ, 0, 1, 0, 1, 1}, "HasCapabilityAmbient"});
  test_cases.push_back({{CAP_AUDIT_READ, 1, 1, 0, 0, 1}, "HasCapabilityAmbient"});
  test_cases.push_back({{CAP_AUDIT_READ, 1, 1, 0, 1, 1}, "HasCapabilityAmbient"});

  test_helper::ForkHelper helper;

  for (const auto &test_case : test_cases) {
    SCOPED_TRACE(fxl::StringPrintf("(Eff Per Inh Bnd Amb) (%d %d %d %d %d)",
                                   test_case.caps.effective, test_case.caps.permitted,
                                   test_case.caps.inheritable, test_case.caps.bounding,
                                   test_case.caps.ambient));
    helper.RunInForkedProcess([test_case]() {
      EXPECT_NONFATAL_FAILURE(SetUpCapabilities(test_case.caps), test_case.failure);
    });
    EXPECT_TRUE(helper.WaitForChildren());
  }
}

TEST_F(CapsExecTest, NonRootUserExecutingNonSUIDProgram) {
  std::vector<capability_t> starting_caps;
  std::vector<capability_t> expected_caps;

  // Effective Permitted Inheritable Bounding Ambient
  //                                      Ef Pe In Bn Am
  starting_caps.push_back({CAP_AUDIT_READ, 0, 0, 0, 0, 0});
  expected_caps.push_back({CAP_AUDIT_READ, 0, 0, 0, 0, 0});

  // NOT POSSIBLE                          0, 0, 0, 0, 1

  starting_caps.push_back({CAP_AUDIT_READ, 0, 0, 0, 1, 0});
  expected_caps.push_back({CAP_AUDIT_READ, 0, 0, 0, 1, 0});

  // NOT POSSIBLE                          0, 0, 0, 1, 1

  starting_caps.push_back({CAP_AUDIT_READ, 0, 0, 1, 0, 0});
  expected_caps.push_back({CAP_AUDIT_READ, 0, 0, 1, 0, 0});

  // NOT POSSIBLE                          0, 0, 1, 0, 1

  starting_caps.push_back({CAP_AUDIT_READ, 0, 0, 1, 1, 0});
  expected_caps.push_back({CAP_AUDIT_READ, 0, 0, 1, 1, 0});

  // NOT POSSIBLE                          0, 0, 1, 1, 1

  starting_caps.push_back({CAP_AUDIT_READ, 0, 1, 0, 0, 0});
  expected_caps.push_back({CAP_AUDIT_READ, 0, 0, 0, 0, 0});

  // NOT POSSIBLE                          0, 1, 0, 0, 1

  starting_caps.push_back({CAP_AUDIT_READ, 0, 1, 0, 1, 0});
  expected_caps.push_back({CAP_AUDIT_READ, 0, 0, 0, 1, 0});

  // NOT POSSIBLE                          0, 1, 0, 1, 1

  starting_caps.push_back({CAP_AUDIT_READ, 0, 1, 1, 0, 0});
  expected_caps.push_back({CAP_AUDIT_READ, 0, 0, 1, 0, 0});

  starting_caps.push_back({CAP_AUDIT_READ, 0, 1, 1, 0, 1});
  expected_caps.push_back({CAP_AUDIT_READ, 1, 1, 1, 0, 1});

  starting_caps.push_back({CAP_AUDIT_READ, 0, 1, 1, 1, 0});
  expected_caps.push_back({CAP_AUDIT_READ, 0, 0, 1, 1, 0});

  starting_caps.push_back({CAP_AUDIT_READ, 0, 1, 1, 1, 1});
  expected_caps.push_back({CAP_AUDIT_READ, 1, 1, 1, 1, 1});

  // NOT POSSIBLE                          1, 0, 0, 0, 0
  // NOT POSSIBLE                          1, 0, 0, 0, 1
  // NOT POSSIBLE                          1, 0, 0, 1, 0
  // NOT POSSIBLE                          1, 0, 0, 1, 1
  // NOT POSSIBLE                          1, 0, 1, 0, 0
  // NOT POSSIBLE                          1, 0, 1, 0, 1
  // NOT POSSIBLE                          1, 0, 1, 1, 0
  // NOT POSSIBLE                          1, 0, 1, 1, 1

  starting_caps.push_back({CAP_AUDIT_READ, 1, 1, 0, 0, 0});
  expected_caps.push_back({CAP_AUDIT_READ, 0, 0, 0, 0, 0});

  // NOT POSSIBLE                          1, 1, 0, 0, 1

  starting_caps.push_back({CAP_AUDIT_READ, 1, 1, 0, 1, 0});
  expected_caps.push_back({CAP_AUDIT_READ, 0, 0, 0, 1, 0});

  // NOT POSSIBLE                          1, 1, 0, 1, 1

  starting_caps.push_back({CAP_AUDIT_READ, 1, 1, 1, 0, 0});
  expected_caps.push_back({CAP_AUDIT_READ, 0, 0, 1, 0, 0});

  starting_caps.push_back({CAP_AUDIT_READ, 1, 1, 1, 0, 1});
  expected_caps.push_back({CAP_AUDIT_READ, 1, 1, 1, 0, 1});

  starting_caps.push_back({CAP_AUDIT_READ, 1, 1, 1, 1, 0});
  expected_caps.push_back({CAP_AUDIT_READ, 0, 0, 1, 1, 0});

  starting_caps.push_back({CAP_AUDIT_READ, 1, 1, 1, 1, 1});
  expected_caps.push_back({CAP_AUDIT_READ, 1, 1, 1, 1, 1});

  for (size_t test_case = 0; test_case < starting_caps.size(); test_case++) {
    SCOPED_TRACE(
        fxl::StringPrintf("(Eff Per Inh Bnd Amb) (%d %d %d %d %d) -> (%d %d %d %d %d)",
                          starting_caps[test_case].effective, starting_caps[test_case].permitted,
                          starting_caps[test_case].inheritable, starting_caps[test_case].bounding,
                          starting_caps[test_case].ambient, expected_caps[test_case].effective,
                          expected_caps[test_case].permitted, expected_caps[test_case].inheritable,
                          expected_caps[test_case].bounding, expected_caps[test_case].ambient));

    std::vector<capability_t> capabilities;
    ASSERT_TRUE(RunPrintCapabilities(
        [&]() {
          SAFE_SYSCALL(setresgid(kUser1Gid, kUser1Gid, kUser1Gid));
          SAFE_SYSCALL(setgroups(0, nullptr));

          // Will not drop permitted capabilities.
          SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, SECBIT_KEEP_CAPS));
          SAFE_SYSCALL(setresuid(kUser1Uid, kUser1Uid, kUser1Uid));
          SetUpCapabilities(starting_caps[test_case]);
          // Drop all other capabilities.
          for (int cap = 0; cap <= cap_last_cap_; cap++) {
            if (cap == starting_caps[test_case].cap_num)
              continue;
            test_helper::UnsetCapabilityEffective(cap);
            test_helper::UnsetCapabilityPermitted(cap);
            test_helper::UnsetCapabilityInheritable(cap);
            test_helper::UnsetCapabilityAmbient(cap);
          }
        },
        capabilities));

    int cap_num = starting_caps[test_case].cap_num;

    EXPECT_EQ(capabilities[cap_num].effective, expected_caps[test_case].effective);
    EXPECT_EQ(capabilities[cap_num].permitted, expected_caps[test_case].permitted);
    EXPECT_EQ(capabilities[cap_num].inheritable, expected_caps[test_case].inheritable);
    EXPECT_EQ(capabilities[cap_num].bounding, expected_caps[test_case].bounding);
    EXPECT_EQ(capabilities[cap_num].ambient, expected_caps[test_case].ambient);
  }
}

TEST_F(CapsExecTest, SecurebitNoRootDropsCapabilitiesOnExec) {
  std::vector<capability_t> capabilities;

  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        SAFE_SYSCALL(setresuid(kRootUid, kRootUid, kRootUid));
        SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, SECBIT_NOROOT));
        ASSERT_EQ(SAFE_SYSCALL(prctl(PR_GET_SECUREBITS)), SECBIT_NOROOT);
      },
      capabilities));

  for (int cap = 0; cap <= cap_last_cap_; cap++) {
    EXPECT_EQ(capabilities[cap].effective, 0);
    EXPECT_EQ(capabilities[cap].permitted, 0);
  }
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   SECBIT_NOROOT
//          If this bit is set, then the kernel does not grant
//          capabilities when [...] a process with an effective or real UID
//          of 0 calls execve(2).
//
//   Executing a program that changes UID or GID due to the set-user-ID or
//   set-group-ID bits or executing a program that has any file capabilities
//   set will clear the ambient set.
//
// Observed:
//
//   The ambient set is also cleared whenever real UID != effective UID prior to
//   execve(2), even when executing an unprivileged binary.
TEST_F(CapsExecTest, RealUidRootEffectiveUidNonRootWithNoRootClearsAmbientCaps) {
  std::vector<capability_t> capabilities;

  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        // 1. Enable SECBIT_NO_SETUID_FIXUP so that changing the EUID to non-root
        //    does not immediately clear our permitted/ambient capabilities.
        // 2. Enable SECBIT_NOROOT so that having real UID == 0 does not
        //    automatically grant us all capabilities upon exec.
        SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, SECBIT_NO_SETUID_FIXUP | SECBIT_NOROOT));

        // 3. Set CAP_NET_ADMIN in the Inheritable and Ambient sets.
        //    (It is already in Permitted because we started as root).
        test_helper::SetCapabilityInheritable(CAP_NET_ADMIN);
        test_helper::SetCapabilityAmbient(CAP_NET_ADMIN);

        // 4. Change effective UID to non-root (kUser1Uid), leaving real UID as root (kRootUid).
        SAFE_SYSCALL(setresuid(kRootUid, kUser1Uid, kRootUid));

        // Verify the pre-exec state
        ASSERT_EQ(getuid(), kRootUid);
        ASSERT_EQ(geteuid(), kUser1Uid);
        ASSERT_TRUE(test_helper::HasCapabilityAmbient(CAP_NET_ADMIN));
      },
      capabilities));

  // After exec:
  // Because SECBIT_NOROOT was set, we should NOT have regained all capabilities.
  // Furthermore, because we transitioned to a state where real UID (0) != effective UID (non-root),
  // the kernel clears the ambient capability set.
  // Therefore, the new process ends up with NO capabilities at all.
  for (int cap = 0; cap <= cap_last_cap_; cap++) {
    EXPECT_EQ(capabilities[cap].effective, 0);
    EXPECT_EQ(capabilities[cap].permitted, 0);
    EXPECT_EQ(capabilities[cap].ambient, 0);
  }
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   P'(effective)   = F(effective) ? P'(permitted) : P'(ambient)
//
//   (1)  If the real or effective user ID of the process is 0 (root), then
//        the file inheritable and permitted sets are ignored; instead they
//        are notionally considered to be all ones [...].
//
//   (2)  If the effective user ID of the process is 0 (root) or the file
//        effective bit is in fact enabled, then the file effective bit is
//        notionally defined to be one (enabled).
//
// Verifies that when real UID is 0 and effective UID is non-zero (without
// SECBIT_NOROOT), rule (1) grants all capabilities in P'(permitted), rule (2)
// leaves F(effective) = 0, and P'(ambient) is cleared because real UID !=
// effective UID, resulting in P'(effective) = 0.
TEST_F(CapsExecTest, RealUidRootEffectiveUidNonRootRegainsAllCapsByDefault) {
  std::vector<capability_t> capabilities;

  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        // Enable SECBIT_NO_SETUID_FIXUP to preserve caps during the EUID change,
        // but leave SECBIT_NOROOT disabled.
        SAFE_SYSCALL(prctl(PR_SET_SECUREBITS, SECBIT_NO_SETUID_FIXUP));

        // Set CAP_NET_ADMIN in Ambient.
        test_helper::SetCapabilityInheritable(CAP_NET_ADMIN);
        test_helper::SetCapabilityAmbient(CAP_NET_ADMIN);

        // Change effective UID to non-root.
        SAFE_SYSCALL(setresuid(kRootUid, kUser1Uid, kRootUid));

        ASSERT_EQ(getuid(), kRootUid);
        ASSERT_EQ(geteuid(), kUser1Uid);
      },
      capabilities));

  // After exec:
  // Because real UID is 0 and SECBIT_NOROOT was NOT set, the kernel automatically
  // grants ALL capabilities to the permitted set of the new process.
  // However, because the effective UID is non-zero and the ambient set was cleared,
  // the effective capability set is empty.
  for (int cap = 0; cap <= cap_last_cap_; cap++) {
    EXPECT_EQ(capabilities[cap].effective, 0);
    EXPECT_EQ(capabilities[cap].permitted, 1);
    EXPECT_EQ(capabilities[cap].ambient, 0);
  }
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   Executing a program that changes UID or GID due to the set-user-ID or
//   set-group-ID bits or executing a program that has any file capabilities
//   set will clear the ambient set.
//
// Verifies that executing a binary with the SUID bit set when owned by the
// caller's own UID does not change the UID, and therefore preserves the
// ambient capability set.
TEST_F(CapsExecTest, SUIDBinarySameUserPreservesAmbientCapabilities) {
  std::vector<capability_t> capabilities;

  // Set print_helper to be SUID owned by kUser1Uid.
  ASSERT_THAT(chown(print_helper_.c_str(), kUser1Uid, kUser1Gid), SyscallSucceeds());
  ASSERT_THAT(chmod(print_helper_.c_str(), S_ISUID | S_IXOTH | S_IRWXU), SyscallSucceeds());

  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        SwitchToUser1(/*keep_caps=*/true);

        // Set CAP_NET_ADMIN in Ambient.
        test_helper::SetCapabilityInheritable(CAP_NET_ADMIN);
        test_helper::SetCapabilityAmbient(CAP_NET_ADMIN);

        // Verify pre-exec state.
        ASSERT_EQ(getuid(), kUser1Uid);
        ASSERT_EQ(geteuid(), kUser1Uid);
        ASSERT_TRUE(test_helper::HasCapabilityAmbient(CAP_NET_ADMIN));
      },
      capabilities));

  // After exec:
  // Because the binary is SUID to the SAME user (kUser1Uid) and there is no
  // UID change, the SUID bit does NOT clear the ambient capability set.
  // Therefore, the ambient capability set is PRESERVED.
  for (int cap = 0; cap <= cap_last_cap_; cap++) {
    if (cap == CAP_NET_ADMIN) {
      EXPECT_EQ(capabilities[cap].effective, 1);
      EXPECT_EQ(capabilities[cap].permitted, 1);
      EXPECT_EQ(capabilities[cap].ambient, 1);
    } else {
      EXPECT_EQ(capabilities[cap].effective, 0);
      EXPECT_EQ(capabilities[cap].permitted, 0);
      EXPECT_EQ(capabilities[cap].ambient, 0);
    }
  }
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   Executing a program that changes UID or GID due to the set-user-ID or
//   set-group-ID bits or executing a program that has any file capabilities
//   set will clear the ambient set.
//
// Verifies that executing a binary whose SUID or SGID bit changes the caller's
// effective UID or GID to another non-root user or group clears the ambient
// capability set.
TEST_F(CapsExecTest, SuidOrSgidBinaryDifferentUserOrGroupClearsAmbientCapabilities) {
  auto setup_caller_with_ambient_cap = []() {
    SwitchToUser1(/*keep_caps=*/true);

    test_helper::SetCapabilityInheritable(CAP_NET_ADMIN);
    test_helper::SetCapabilityAmbient(CAP_NET_ADMIN);
    ASSERT_TRUE(test_helper::HasCapabilityAmbient(CAP_NET_ADMIN));
  };

  // Case 1: SUID to a different non-root user (kUser2Uid) clears ambient capabilities.
  ASSERT_THAT(chown(print_helper_.c_str(), kUser2Uid, kUser1Gid), SyscallSucceeds());
  ASSERT_THAT(
      chmod(print_helper_.c_str(), S_ISUID | S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH),
      SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities(setup_caller_with_ambient_cap, capabilities));
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].ambient, 0);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 0);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 0);

  // Case 2: SGID to a different group (kUser2Gid) clears ambient capabilities.
  ASSERT_THAT(chown(print_helper_.c_str(), kUser1Uid, kUser2Gid), SyscallSucceeds());
  ASSERT_THAT(
      chmod(print_helper_.c_str(), S_ISGID | S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH),
      SyscallSucceeds());

  ASSERT_TRUE(RunPrintCapabilities(setup_caller_with_ambient_cap, capabilities));
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].ambient, 0);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 0);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 0);
}

TEST(CapsTest, CapsetMasksReservedBits) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities. skipping.";
  }

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([]() {
    // 1. Get current capabilities using the V3 (64-bit) ABI.
    __user_cap_header_struct header{};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    header.pid = 0;  // 0 means the calling thread

    __user_cap_data_struct caps[_LINUX_CAPABILITY_U32S_3]{};
    ASSERT_THAT(syscall(SYS_capget, &header, caps), SyscallSucceeds());

    // Reconstruct 64-bit masks from the two u32 halves.
    uint64_t orig_permitted =
        static_cast<uint64_t>(caps[0].permitted) | (static_cast<uint64_t>(caps[1].permitted) << 32);
    uint64_t orig_effective =
        static_cast<uint64_t>(caps[0].effective) | (static_cast<uint64_t>(caps[1].effective) << 32);
    uint64_t orig_inheritable = static_cast<uint64_t>(caps[0].inheritable) |
                                (static_cast<uint64_t>(caps[1].inheritable) << 32);

    // 2. Determine the last valid capability supported by the running kernel.
    int cap_last_cap = 0;
    FILE *fp = fopen("/proc/sys/kernel/cap_last_cap", "r");
    ASSERT_NE(fp, nullptr);
    ASSERT_EQ(fscanf(fp, "%d\n", &cap_last_cap), 1);
    fclose(fp);

    // 3. Construct masks for valid vs reserved bits.
    uint64_t valid_mask = (1ULL << (cap_last_cap + 1)) - 1;
    uint64_t reserved_mask = ~valid_mask;

    // We attempt to set reserved bits. To avoid EPERM, we only set valid bits
    // that we already possess, plus the reserved bits (which should be ignored).
    uint64_t test_permitted = orig_permitted | reserved_mask;
    uint64_t test_effective = orig_effective | reserved_mask;
    uint64_t test_inheritable = orig_inheritable | reserved_mask;

    __user_cap_data_struct set_caps[_LINUX_CAPABILITY_U32S_3]{};
    set_caps[0].permitted = static_cast<uint32_t>(test_permitted);
    set_caps[1].permitted = static_cast<uint32_t>(test_permitted >> 32);
    set_caps[0].effective = static_cast<uint32_t>(test_effective);
    set_caps[1].effective = static_cast<uint32_t>(test_effective >> 32);
    set_caps[0].inheritable = static_cast<uint32_t>(test_inheritable);
    set_caps[1].inheritable = static_cast<uint32_t>(test_inheritable >> 32);

    // This call must succeed because reserved bits should be silently dropped.
    EXPECT_THAT(syscall(SYS_capset, &header, set_caps), SyscallSucceeds());

    // 4. Get capabilities again to verify the kernel masked the reserved bits.
    __user_cap_data_struct get_caps[_LINUX_CAPABILITY_U32S_3]{};
    ASSERT_THAT(syscall(SYS_capget, &header, get_caps), SyscallSucceeds());

    uint64_t got_permitted = static_cast<uint64_t>(get_caps[0].permitted) |
                             (static_cast<uint64_t>(get_caps[1].permitted) << 32);
    uint64_t got_effective = static_cast<uint64_t>(get_caps[0].effective) |
                             (static_cast<uint64_t>(get_caps[1].effective) << 32);
    uint64_t got_inheritable = static_cast<uint64_t>(get_caps[0].inheritable) |
                               (static_cast<uint64_t>(get_caps[1].inheritable) << 32);

    // Assert that no reserved bits are set in any of the sets.
    EXPECT_EQ(got_permitted & reserved_mask, 0ULL);
    EXPECT_EQ(got_effective & reserved_mask, 0ULL);
    EXPECT_EQ(got_inheritable & reserved_mask, 0ULL);

    // Assert that the original valid capabilities are preserved.
    EXPECT_EQ(got_permitted & valid_mask, orig_permitted & valid_mask);
    EXPECT_EQ(got_effective & valid_mask, orig_effective & valid_mask);
    EXPECT_EQ(got_inheritable & valid_mask, orig_inheritable & valid_mask);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   During an execve(2), the kernel calculates the new capabilities of the
//   process using the following algorithm:
//
//       P'(ambient)     = (file is privileged) ? 0 : P(ambient)
//
//       P'(permitted)   = (P(inheritable) & F(inheritable)) |
//                         (F(permitted) & P(bounding)) | P'(ambient)
//
//       P'(effective)   = F(effective) ? P'(permitted) : P'(ambient)
//
// Verifies that executing a binary with V2 effective file capabilities
// grants those capabilities in both the permitted and effective sets of a non-root user.
TEST_F(CapsExecTest, FileV2EffectiveCapsGrantsPermittedAndEffective) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN) | (1ULL << CAP_AUDIT_READ);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities([]() { SwitchToUser1(); }, capabilities));

  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 1);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].permitted, 1);
  EXPECT_EQ(capabilities[CAP_AUDIT_READ].effective, 1);
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//       P'(permitted)   = (P(inheritable) & F(inheritable)) |
//                         (F(permitted) & P(bounding)) | P'(ambient)
//
//       P'(effective)   = F(effective) ? P'(permitted) : P'(ambient)
//
// Verifies that executing a binary with V2 file capabilities without the
// effective bit set (F(effective) = 0) grants the capabilities in the permitted
// set, but leaves the effective set empty (since P'(ambient) is 0).
TEST_F(CapsExecTest, FileV2NotEffectiveCapsGrantsPermittedOnly) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/false),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities([]() { SwitchToUser1(); }, capabilities));

  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 0);
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   As with a binary that has VFS_CAP_REVISION_2 file capabilities, a binary
//   with VFS_CAP_REVISION_3 file capabilities confers capabilities to a
//   process during execve().  However, capabilities are conferred only if
//   the binary is executed by a process that resides in a user namespace
//   whose UID 0 maps to the root user ID that is saved in the extended
//   attribute, or when executed by a process that resides in a descendant of
//   such a namespace.
//
// Verifies that executing a binary with V3 file capabilities whose rootid
// matches the root of the user namespace (0) successfully grants the capabilities.
TEST_F(CapsExecTest, FileV3MatchingRootIdGrantsPermittedAndEffective) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  // rootid = 0 matches the root of our namespace.
  ASSERT_THAT(SetFileCapabilitiesV3(print_helper_, caps, /*inheritable=*/0, /*effective=*/true,
                                    /*rootid=*/0),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities([]() { SwitchToUser1(); }, capabilities));

  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 1);
}

// Verifies that executing a binary with V3 file capabilities whose rootid (9999)
// matches the root of a non-initial user namespace confers capabilities to a
// non-root process inside that user namespace.
TEST_F(CapsExecTest, FileV3MatchingRootIdGrantsCapsInsideUserNamespace) {
  if (!ProbeUserNamespaceSupport()) {
    GTEST_SKIP() << "User namespaces (CLONE_NEWUSER) not supported in this environment.";
  }

  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  ASSERT_THAT(SetFileCapabilitiesV3(print_helper_, caps, /*inheritable=*/0, /*effective=*/true,
                                    /*rootid=*/9999),
              SyscallSucceeds());

  test_helper::ForkHelper helper;
  test_helper::Rendezvous child_unshared = test_helper::MakeRendezvous();
  test_helper::Rendezvous parent_mapped = test_helper::MakeRendezvous();

  pid_t pid = helper.RunInForkedProcess([&]() {
    parent_mapped.poker = test_helper::Poker();

    ASSERT_THAT(setresgid(9999, 9999, 9999), SyscallSucceeds());
    ASSERT_THAT(setresuid(9999, 9999, 9999), SyscallSucceeds());
    ASSERT_THAT(unshare(CLONE_NEWUSER), SyscallSucceeds());

    child_unshared.poker.poke();
    parent_mapped.holder.hold();

    // Switch to non-root UID 1 inside the user namespace (mapped to host UID 10000).
    std::vector<capability_t> ns_caps;
    ASSERT_TRUE(RunPrintCapabilities(
        []() {
          ASSERT_THAT(setresgid(1, 1, 1), SyscallSucceeds());
          ASSERT_THAT(setresuid(1, 1, 1), SyscallSucceeds());
        },
        ns_caps));
    EXPECT_EQ(ns_caps[CAP_NET_ADMIN].permitted, 1);
    EXPECT_EQ(ns_caps[CAP_NET_ADMIN].effective, 1);
  });

  child_unshared.poker = test_helper::Poker();
  child_unshared.holder.hold();

  std::string uid_map_path = "/proc/" + std::to_string(pid) + "/uid_map";
  std::string gid_map_path = "/proc/" + std::to_string(pid) + "/gid_map";
  ASSERT_TRUE(files::WriteFile(uid_map_path, "0 9999 2\n"));
  ASSERT_TRUE(files::WriteFile(gid_map_path, "0 9999 2\n"));

  parent_mapped.poker.poke();
  EXPECT_TRUE(helper.WaitForChildren());
}

// Verifies that executing a binary with V3 file capabilities whose rootid
// does not match the root of the user namespace (0) causes the capabilities
// to be ignored.
TEST_F(CapsExecTest, FileV3NonMatchingRootIdIgnoresCapabilities) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  // rootid = 9999 (does not match 0 of the host/initial namespace).
  ASSERT_THAT(SetFileCapabilitiesV3(print_helper_, caps, /*inheritable=*/0, /*effective=*/true,
                                    /*rootid=*/9999),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities([]() { SwitchToUser1(); }, capabilities));

  // The capability must be ignored (not obtained).
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 0);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 0);
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   If (a) the binary that is being executed has capabilities attached and
//   (b) the real user ID of the process is not 0 (root) and (c) the
//   effective user ID of the process is 0 (root), then the file capability
//   bits are honored (i.e., they are not notionally considered to be all
//   ones).
//
// Verifies that when a non-root user executes a set-user-ID-root binary that
// has file capabilities attached, the file capabilities are honored instead of
// granting all root capabilities.
TEST_F(CapsExecTest, SuidRootBinaryWithFileCapabilitiesHonorsFileCapabilities) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  ASSERT_THAT(chown(print_helper_.c_str(), 0, 0), SyscallSucceeds());
  ASSERT_THAT(chmod(print_helper_.c_str(), S_ISUID | S_IRWXU | S_IXGRP | S_IXOTH),
              SyscallSucceeds());
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities([]() { SwitchToUser1(); }, capabilities));

  // The file capability (CAP_NET_ADMIN) must be granted.
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 1);

  // Other root capabilities (e.g. CAP_SYS_ADMIN) must NOT be granted despite SUID-root.
  EXPECT_EQ(capabilities[CAP_SYS_ADMIN].permitted, 0);
  EXPECT_EQ(capabilities[CAP_SYS_ADMIN].effective, 0);
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   Note that one can assign empty capability sets to a program file, and
//   thus it is possible to create a set-user-ID-root program that changes
//   the effective and saved set-user-ID of the process that executes the
//   program to 0, but confers no capabilities to that process.
//
// Verifies that when a non-root user executes a set-user-ID-root binary with
// an empty file capability set, its effective and saved set-user-ID become 0
// while no capabilities are conferred.
TEST_F(CapsExecTest, SuidRootBinaryWithEmptyFileCapabilitiesConfersNoCapabilities) {
  ASSERT_THAT(chown(print_helper_.c_str(), 0, 0), SyscallSucceeds());
  ASSERT_THAT(chmod(print_helper_.c_str(), S_ISUID | S_IRWXU | S_IXGRP | S_IXOTH),
              SyscallSucceeds());
  ASSERT_THAT(
      SetFileCapabilitiesV2(print_helper_, /*permitted=*/0, /*inheritable=*/0, /*effective=*/false),
      SyscallSucceeds());

  uid_t ruid = 999, euid = 999, suid = 999;
  ASSERT_TRUE(RunPrintUids([]() { SwitchToUser1(); }, &ruid, &euid, &suid));
  EXPECT_EQ(ruid, kUser1Uid);
  EXPECT_EQ(euid, 0u);
  EXPECT_EQ(suid, 0u);

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities([]() { SwitchToUser1(); }, capabilities));
  for (int cap = 0; cap <= cap_last_cap_; cap++) {
    EXPECT_EQ(capabilities[cap].permitted, 0);
    EXPECT_EQ(capabilities[cap].effective, 0);
  }
}

// From <https://man7.org/linux/man-pages/man3/getauxval.3.html>:
//
//   AT_SECURE
//          Has a nonzero value if this executable should be treated
//          securely.  Most commonly, a nonzero value indicates that the
//          process is executing a set-user-ID or set-group-ID binary, or
//          that it gained capabilities by executing a binary file that has
//          capabilities (see capabilities(7)).
//
// Verifies that executing a binary with file capabilities that grant new
// capabilities to an unprivileged caller sets AT_SECURE in the auxiliary vector.
TEST_F(CapsExecTest, FileCapsGrantingNewCapsSetsAtSecure) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());

  unsigned long at_secure = 0;
  ASSERT_TRUE(RunPrintAtSecure([]() { SwitchToUser1(); }, &at_secure));

  EXPECT_EQ(at_secure, 1UL);
}

// Verifies that executing a binary with an all-zero file capability attribute
// (where F(effective) is false and P'(permitted) equals P'(ambient)) does NOT
// set AT_SECURE.
TEST_F(CapsExecTest, FileCapsZeroDoesNotSetAtSecure) {
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, /*permitted=*/0, /*inheritable=*/0,
                                    /*effective=*/false),
              SyscallSucceeds());

  unsigned long at_secure = 1;
  ASSERT_TRUE(RunPrintAtSecure([]() { SwitchToUser1(); }, &at_secure));

  EXPECT_EQ(at_secure, 0UL);
}

// Verifies that a root process (real UID == 0) executing a binary with file
// capabilities does not set AT_SECURE because root already possesses all
// capabilities and does not gain new capabilities from the file.
TEST_F(CapsExecTest, RootExecutingBinaryWithFileCapsDoesNotSetAtSecure) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());

  unsigned long at_secure = 1;
  ASSERT_TRUE(RunPrintAtSecure([]() {}, &at_secure));

  EXPECT_EQ(at_secure, 0UL);
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   The kernel recognizes a file which has the file effective bit set as
//   capability-dumb for the purpose of the check described here.  When
//   executing a capability-dumb binary, the kernel checks if the process
//   obtained all permitted capabilities that were specified in the file
//   permitted set [...].  If the process did not obtain the full set of file
//   permitted capabilities, then execve(2) fails with the error EPERM.
//
// Verifies that executing a binary with F(effective) set fails with EPERM if
// the caller's bounding set excludes any capability in F(permitted).
TEST_F(CapsExecTest, FileEffectiveCapsFailsWhenBoundingSetExcludesPermitted) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());

  RunExecExpectError(
      [&]() {
        test_helper::UnsetCapabilityBounding(CAP_NET_ADMIN);
        SwitchToUser1();
      },
      EPERM);
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   When executing a capability-dumb binary, the kernel checks if the
//   process obtained all permitted capabilities that were specified in the
//   file permitted set [...].  If the process did not obtain the full set of
//   file permitted capabilities, then execve(2) fails with the error EPERM.
//
// From <https://man7.org/linux/man-pages/man2/execve.2.html>:
//
//   The set-user-ID and set-group-ID mode bits of a program are ignored if
//   any of the following is true:
//   •  the no_new_privs attribute is set for the calling thread;
//   ...
//   The capabilities of the program file (see capabilities(7)) are also
//   ignored if any of the above are true.
//
// Observed:
//
//   Under PR_SET_NO_NEW_PRIVS, rather than treating file capabilities as empty
//   or failing with EPERM for a capability-dumb binary, execve(2) intersects
//   P'(permitted) with P(permitted), retaining file capabilities already held
//   by the caller while masking out unheld file capabilities.
TEST_F(CapsExecTest, FileEffectiveCapsMaskedByNoNewPrivs) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN) | (1ULL << CAP_NET_RAW);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        SwitchToUser1(/*keep_caps=*/true);
        // Hold CAP_NET_ADMIN in permitted/effective prior to execve(2), but not CAP_NET_RAW.
        test_helper::UnsetCapabilityEffective(CAP_NET_RAW);
        test_helper::UnsetCapabilityPermitted(CAP_NET_RAW);
        test_helper::SetCapabilityPermitted(CAP_NET_ADMIN);
        test_helper::SetCapabilityEffective(CAP_NET_ADMIN);
        SAFE_SYSCALL(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0));
      },
      capabilities));

  // CAP_NET_ADMIN was held prior to execve(2), so it is preserved and raised to effective.
  // CAP_NET_RAW was not held prior to execve(2), so it is masked out by no_new_privs.
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 1);
  EXPECT_EQ(capabilities[CAP_NET_RAW].permitted, 0);
  EXPECT_EQ(capabilities[CAP_NET_RAW].effective, 0);
}

// Verifies that when PR_SET_NO_NEW_PRIVS is set and a binary has non-effective
// file capabilities (F(effective) = 0), capabilities already held in
// P(permitted) prior to execve(2) are retained in P'(permitted), while unheld
// file capabilities are masked out.
TEST_F(CapsExecTest, FileNotEffectiveCapsMaskedByNoNewPrivs) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN) | (1ULL << CAP_NET_RAW);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/false),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        SwitchToUser1(/*keep_caps=*/true);
        // Hold CAP_NET_ADMIN in permitted/effective prior to execve(2), but not CAP_NET_RAW.
        test_helper::UnsetCapabilityEffective(CAP_NET_RAW);
        test_helper::UnsetCapabilityPermitted(CAP_NET_RAW);
        test_helper::SetCapabilityPermitted(CAP_NET_ADMIN);
        test_helper::SetCapabilityEffective(CAP_NET_ADMIN);
        SAFE_SYSCALL(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0));
      },
      capabilities));

  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 0);
  EXPECT_EQ(capabilities[CAP_NET_RAW].permitted, 0);
  EXPECT_EQ(capabilities[CAP_NET_RAW].effective, 0);
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   Executing a program that changes UID or GID due to the set-user-ID or
//   set-group-ID bits or executing a program that has any file capabilities
//   set will clear the ambient set.
//
// Verifies that executing a binary with any security.capability extended
// attribute clears P'(ambient), even when all file capability sets are empty.
TEST_F(CapsExecTest, FileCapabilitiesClearAmbientCapabilities) {
  ASSERT_THAT(
      SetFileCapabilitiesV2(print_helper_, /*permitted=*/0, /*inheritable=*/0, /*effective=*/false),
      SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        SwitchToUser1(/*keep_caps=*/true);

        test_helper::SetCapabilityPermitted(CAP_NET_ADMIN);
        test_helper::SetCapabilityEffective(CAP_NET_ADMIN);
        test_helper::SetCapabilityInheritable(CAP_NET_ADMIN);
        test_helper::SetCapabilityAmbient(CAP_NET_ADMIN);
        ASSERT_TRUE(test_helper::HasCapabilityAmbient(CAP_NET_ADMIN));
      },
      capabilities));

  EXPECT_EQ(capabilities[CAP_NET_ADMIN].ambient, 0);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 0);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 0);
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   P'(permitted)   = (P(inheritable) & F(inheritable)) |
//                     (F(permitted) & P(bounding)) | P'(ambient)
//
//   P'(effective)   = F(effective) ? P'(permitted) : P'(ambient)
//
// Verifies that F(inheritable) grants capabilities into P'(permitted) (and
// P'(effective) when F(effective) is true) only for capabilities that are also
// present in the caller's pre-exec P(inheritable) set.
TEST_F(CapsExecTest, FileInheritableCapsGrantedOnlyWhenCallerInheritableSet) {
  uint64_t inh_caps = (1ULL << CAP_NET_ADMIN) | (1ULL << CAP_NET_RAW);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, /*permitted=*/0, /*inheritable=*/inh_caps,
                                    /*effective=*/true),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        SwitchToUser1(/*keep_caps=*/true);

        // Place CAP_NET_ADMIN (but not CAP_NET_RAW) in P(inheritable).
        test_helper::SetCapabilityPermitted(CAP_NET_ADMIN);
        test_helper::SetCapabilityInheritable(CAP_NET_ADMIN);

        // Drop all permitted/effective capabilities before exec.
        test_helper::UnsetCapabilityEffective(CAP_NET_ADMIN);
        test_helper::UnsetCapabilityPermitted(CAP_NET_ADMIN);
      },
      capabilities));

  // CAP_NET_ADMIN was in P(inheritable) & F(inheritable), so it is granted.
  // CAP_NET_RAW was in F(inheritable) but not P(inheritable), so it is not granted.
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].inheritable, 1);
  EXPECT_EQ(capabilities[CAP_NET_RAW].permitted, 0);
  EXPECT_EQ(capabilities[CAP_NET_RAW].effective, 0);
}

// From <https://man7.org/linux/man-pages/man2/execve.2.html>:
//
//   The set-user-ID and set-group-ID mode bits of a program are ignored if
//   any of the following is true:
//   ...
//   •  the underlying filesystem is mounted nosuid (the MS_NOSUID flag for
//      mount(2));
//   ...
//   The capabilities of the program file (see capabilities(7)) are also
//   ignored if any of the above are true.
//
// Verifies that when the filesystem is mounted with MS_NOSUID, file
// capabilities are ignored during execve(2).
TEST_F(CapsExecTest, NosuidMountIgnoresFileCapabilities) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());

  ASSERT_THAT(mount(nullptr, path().c_str(), nullptr, MS_REMOUNT | MS_NOSUID, nullptr),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities([]() { SwitchToUser1(); }, capabilities));

  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 0);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 0);
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   The kernel recognizes a file which has the file effective bit set as
//   capability-dumb [...].  If the process did not obtain the full set of file
//   permitted capabilities, then execve(2) fails with the error EPERM.
//
// From <https://man7.org/linux/man-pages/man2/execve.2.html>:
//
//   The set-user-ID and set-group-ID mode bits of a program are ignored
//   if any of the following is true:
//   [...]
//   •  the process is being traced, the tracer does not have the
//      CAP_SYS_PTRACE capability in the user namespace of the
//      tracee's process [...].
//
//   The capabilities of the program file (see capabilities(7)) are also
//   ignored if any of the above are true.
//
// Observed:
//
//   When traced without CAP_SYS_PTRACE, rather than treating file capabilities
//   as empty or failing with EPERM, execve(2) intersects P'(permitted) with
//   P(permitted), retaining file capabilities already held by the tracee while
//   masking out unheld file capabilities.
TEST_F(CapsExecTest, UnprivilegedPtraceIgnoresFileCapabilities) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN) | (1ULL << CAP_NET_RAW);

  auto attach_unprivileged_tracer = []() {
    SwitchToUser1(/*keep_caps=*/true);
    // Tracee holds CAP_NET_ADMIN in permitted/effective, but not CAP_NET_RAW.
    test_helper::UnsetCapabilityEffective(CAP_NET_RAW);
    test_helper::UnsetCapabilityPermitted(CAP_NET_RAW);
    test_helper::SetCapabilityPermitted(CAP_NET_ADMIN);
    test_helper::SetCapabilityEffective(CAP_NET_ADMIN);
    AttachTracerToCurrentProcess();
  };

  // Case 1: F(effective) = false -> held capability (CAP_NET_ADMIN) is preserved in
  // permitted, while unheld capability (CAP_NET_RAW) is masked out.
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/false),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities(attach_unprivileged_tracer, capabilities));
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 0);
  EXPECT_EQ(capabilities[CAP_NET_RAW].permitted, 0);
  EXPECT_EQ(capabilities[CAP_NET_RAW].effective, 0);

  // Case 2: F(effective) = true -> execve(2) succeeds without EPERM; held capability
  // (CAP_NET_ADMIN) is preserved in permitted and effective, while unheld capability
  // (CAP_NET_RAW) is masked out.
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());

  ASSERT_TRUE(RunPrintCapabilities(attach_unprivileged_tracer, capabilities));
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 1);
  EXPECT_EQ(capabilities[CAP_NET_RAW].permitted, 0);
  EXPECT_EQ(capabilities[CAP_NET_RAW].effective, 0);
}

// From <https://man7.org/linux/man-pages/man2/execve.2.html>:
//
//   The set-user-ID and set-group-ID mode bits of a program are ignored if
//   any of the following is true:
//   ...
//   •  the process is being traced, the tracer does not have the
//      CAP_SYS_PTRACE capability in the user namespace of the tracee's
//      process, and the tracing process does not share its user namespace
//      with the tracee.
//
//   The capabilities of the program file (see capabilities(7)) are also
//   ignored if any of the above are true.
TEST_F(CapsExecTest, PrivilegedPtraceHonorsFileCapabilities) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities(
      []() {
        AttachTracerToCurrentProcess();
        SwitchToUser1();
      },
      capabilities));

  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 1);
}

// Verifies that if a process holds an ambient capability that is not currently
// in its effective set, executing a binary without file capabilities raises
// P'(effective) from P'(ambient) without setting AT_SECURE (since no
// capabilities were gained from the file).
TEST_F(CapsExecTest, RaisingCapsViaAmbientDoesNotSetAtSecure) {
  unsigned long at_secure = 999;
  ASSERT_TRUE(RunPrintAtSecure(
      [&]() {
        SwitchToUser1(/*keep_caps=*/true);

        test_helper::SetCapabilityPermitted(CAP_NET_ADMIN);
        test_helper::SetCapabilityInheritable(CAP_NET_ADMIN);
        test_helper::SetCapabilityAmbient(CAP_NET_ADMIN);
        // Leave CAP_NET_ADMIN unset in P(effective) before execve(2).
      },
      &at_secure));

  EXPECT_EQ(at_secure, 0UL);
}

// Verifies that when a non-root caller executes a binary with non-empty file
// capabilities, AT_SECURE is set to 1 even if the caller already held those
// capabilities in P(permitted) and P(effective) prior to execve(2) (since
// non-ambient capabilities would otherwise have been dropped by execve(2)).
struct NonRootAtSecureParam {
  std::string name;
  bool file_effective;
  bool hold_caps_before_exec;
};

class NonRootAtSecureTest : public CapsExecTest,
                            public ::testing::WithParamInterface<NonRootAtSecureParam> {};

TEST_P(NonRootAtSecureTest, SetsAtSecureForAnyFileCaps) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  const auto &param = GetParam();
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0,
                                    /*effective=*/param.file_effective),
              SyscallSucceeds());
  unsigned long at_secure = 999;
  ASSERT_TRUE(RunPrintAtSecure(
      [&]() {
        SwitchToUser1(/*keep_caps=*/param.hold_caps_before_exec);
        if (param.hold_caps_before_exec) {
          test_helper::SetCapabilityPermitted(CAP_NET_ADMIN);
          test_helper::SetCapabilityEffective(CAP_NET_ADMIN);
        }
      },
      &at_secure));
  EXPECT_EQ(at_secure, 1UL);
}

INSTANTIATE_TEST_SUITE_P(NonRootAtSecure, NonRootAtSecureTest,
                         ::testing::Values(
                             NonRootAtSecureParam{
                                 .name = "NonEffectiveFileCapsWithHeldCaps",
                                 .file_effective = false,
                                 .hold_caps_before_exec = true,
                             },
                             NonRootAtSecureParam{
                                 .name = "NonEffectiveFileCapsWithoutHeldCaps",
                                 .file_effective = false,
                                 .hold_caps_before_exec = false,
                             },
                             NonRootAtSecureParam{
                                 .name = "EffectiveFileCapsWithHeldCaps",
                                 .file_effective = true,
                                 .hold_caps_before_exec = true,
                             }),
                         [](const ::testing::TestParamInfo<NonRootAtSecureParam> &info) {
                           return info.param.name;
                         });

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   The kernel recognizes a file which has the file effective bit set as
//   capability-dumb for the purpose of the check described here.  When
//   executing a capability-dumb binary, the kernel checks if the process
//   obtained all permitted capabilities that were specified in the file
//   permitted set [...].  If the process did not obtain the full set of file
//   permitted capabilities, then execve(2) fails with the error EPERM.
//
// Verifies that root executing a capability-aware binary (F(effective) = false)
// succeeds even when P(bounding) excludes a capability in F(permitted), because
// the EPERM check only applies to binaries with F(effective) = true.
TEST_F(CapsExecTest, RootExecutingNonEffectiveFileCapsWithRestrictedBoundingSetSucceeds) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/false),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        // Drop CAP_NET_ADMIN from the bounding set as root (UID 0).
        test_helper::UnsetCapabilityBounding(CAP_NET_ADMIN);
      },
      capabilities));

  // Root gains all capabilities in P(bounding), so CAP_NET_ADMIN is masked out
  // while other capabilities remain granted.
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 0);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 0);
  EXPECT_EQ(capabilities[CAP_CHOWN].permitted, 1);
  EXPECT_EQ(capabilities[CAP_CHOWN].effective, 1);
}

// Verifies that a non-root user executing a capability-aware binary
// (F(effective) = false) succeeds even when P(bounding) excludes a capability
// in F(permitted), because the EPERM check only applies to capability-dumb
// binaries (F(effective) = true).
TEST_F(CapsExecTest, NonRootExecutingNonEffectiveFileCapsWithRestrictedBoundingSetSucceeds) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, caps, /*inheritable=*/0, /*effective=*/false),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        test_helper::UnsetCapabilityBounding(CAP_NET_ADMIN);
        SwitchToUser1();
      },
      capabilities));

  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 0);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 0);
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   Note that the bounding set masks the file permitted capabilities, but
//   not the inheritable capabilities.  If a thread maintains a capability in
//   its inheritable set that is not in its bounding set, then it can still
//   gain that capability in its permitted set by executing a file that has
//   the capability in its inheritable set.
//
// Verifies that executing a capability-dumb binary (F(effective) = true) with
// CAP_NET_ADMIN in both F(permitted) and F(inheritable) succeeds when
// CAP_NET_ADMIN is dropped from P(bounding) but held in P(inheritable), because
// P'(permitted) still obtains the full F(permitted) set via
// (P(inheritable) & F(inheritable)).
TEST_F(CapsExecTest, FileEffectiveCapsSucceedsWhenBoundingSetExcludedCapObtainedViaInheritable) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  ASSERT_THAT(SetFileCapabilitiesV2(print_helper_, /*permitted=*/caps, /*inheritable=*/caps,
                                    /*effective=*/true),
              SyscallSucceeds());

  std::vector<capability_t> capabilities;
  ASSERT_TRUE(RunPrintCapabilities(
      [&]() {
        SwitchToUser1(/*keep_caps=*/true);

        // Add CAP_NET_ADMIN to P(inheritable) before dropping it from P(bounding).
        test_helper::SetCapabilityInheritable(CAP_NET_ADMIN);
        test_helper::SetCapabilityEffective(CAP_SETPCAP);
        test_helper::UnsetCapabilityBounding(CAP_NET_ADMIN);
      },
      capabilities));

  EXPECT_EQ(capabilities[CAP_NET_ADMIN].permitted, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].effective, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].inheritable, 1);
  EXPECT_EQ(capabilities[CAP_NET_ADMIN].bounding, 0);
}

namespace {

class FileCapTest : public ::testing::Test {
 protected:
  void SetUp() override {
    if (!test_helper::HasSysAdmin()) {
      GTEST_SKIP() << "Not running with sysadmin capabilities. skipping.";
    }

    // Mount tmpfs to ensure we have a filesystem that supports extended attributes.
    mount_ = ASSERT_RESULT_SUCCESS_AND_RETURN(
        test_helper::ScopedMount::Mount("", temp_dir_.path(), "tmpfs", 0, nullptr));

    constexpr int kDirPerms = S_IRWXU | S_IXGRP | S_IXOTH;
    ASSERT_THAT(chmod(temp_dir_.path().c_str(), kDirPerms), SyscallSucceeds());
  }

  std::filesystem::path path() const { return temp_dir_.path(); }

  test_helper::ScopedTempDir temp_dir_;
  test_helper::ScopedMount mount_;
};

}  // namespace

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   VFS_CAP_REVISION_3 (since Linux 4.14)
//          As with version 2 file capabilities, version 3 capability masks
//          are 64 bits in size.  But in addition, the root user ID of
//          namespace is encoded in the security.capability extended
//          attribute.
TEST_F(FileCapTest, FileCapabilityV3NonMatchingRootIdPreservedAsV3) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN) | (1ULL << CAP_AUDIT_READ);
  std::string test_file = (path() / "test_file").string();
  ASSERT_TRUE(files::WriteFile(test_file, ""));

  // Write V3 capability (24 bytes) with rootid = 9999.
  ASSERT_THAT(SetFileCapabilitiesV3(test_file, caps, /*inheritable=*/0, /*effective=*/true,
                                    /*rootid=*/9999),
              SyscallSucceeds());

  // Read it back. In the initial namespace, it should remain V3 (24 bytes)
  // because the rootid (9999) does not match the reader's namespace root (0).
  EXPECT_THAT(getxattr(test_file.c_str(), "security.capability", nullptr, 0),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(vfs_ns_cap_data))));
  vfs_cap_data too_small{};
  EXPECT_THAT(getxattr(test_file.c_str(), "security.capability", &too_small, sizeof(too_small)),
              SyscallFailsWithErrno(ERANGE));

  vfs_ns_cap_data read_data{};
  ASSERT_THAT(getxattr(test_file.c_str(), "security.capability", &read_data, sizeof(read_data)),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(vfs_ns_cap_data))));

  uint32_t expected_magic = VFS_CAP_REVISION_3 | VFS_CAP_FLAGS_EFFECTIVE;
  EXPECT_EQ(le32toh(read_data.magic_etc), expected_magic);
  EXPECT_EQ(le32toh(read_data.rootid), 9999u);

  EXPECT_EQ(le32toh(read_data.data[0].permitted), static_cast<uint32_t>(caps));
  EXPECT_EQ(le32toh(read_data.data[1].permitted), static_cast<uint32_t>(caps >> 32u));
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   By contrast, creating or modifying a security.capability extended
//   attribute from a privileged (CAP_SETFCAP) thread that resides in the
//   namespace where the underlying filesystem was mounted (this normally
//   means the initial user namespace) automatically results in the creation
//   of a version 2 (VFS_CAP_REVISION_2) attribute.
//
// Verifies that writing a V3 capability attribute with rootid = 0 in the
// initial namespace is coerced to a 20-byte VFS_CAP_REVISION_2 attribute.
TEST_F(FileCapTest, FileCapabilityV3MatchingRootIdCoercedToV2) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN) | (1ULL << CAP_AUDIT_READ);
  std::string test_file = (path() / "test_file").string();
  ASSERT_TRUE(files::WriteFile(test_file, ""));

  // Write V3 capability (24 bytes) with rootid = 0.
  ASSERT_THAT(
      SetFileCapabilitiesV3(test_file, caps, /*inheritable=*/0, /*effective=*/true, /*rootid=*/0),
      SyscallSucceeds());

  // Read it back. It should be coerced to V2 (20 bytes).
  EXPECT_THAT(getxattr(test_file.c_str(), "security.capability", nullptr, 0),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(vfs_cap_data))));
  char short_buf[sizeof(vfs_cap_data) - 1];
  EXPECT_THAT(getxattr(test_file.c_str(), "security.capability", short_buf, sizeof(short_buf)),
              SyscallFailsWithErrno(ERANGE));

  vfs_cap_data read_data{};
  ASSERT_THAT(getxattr(test_file.c_str(), "security.capability", &read_data, sizeof(read_data)),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(vfs_cap_data))));

  uint32_t expected_magic = VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE;
  EXPECT_EQ(le32toh(read_data.magic_etc), expected_magic);

  EXPECT_EQ(le32toh(read_data.data[0].permitted), static_cast<uint32_t>(caps));
  EXPECT_EQ(le32toh(read_data.data[1].permitted), static_cast<uint32_t>(caps >> 32u));
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   VFS_CAP_REVISION_2 (since Linux 2.6.25)
//          This version allows for file capability masks that are 64 bits in
//          size, and was necessary as the number of supported capabilities
//          grew beyond 32.
//
// Verifies that writing and reading a V2 capability preserves its size and magic.
TEST_F(FileCapTest, FileCapabilityV2Preserved) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  std::string test_file = (path() / "test_file").string();
  ASSERT_TRUE(files::WriteFile(test_file, ""));

  // Write V2 capability (20 bytes).
  ASSERT_THAT(SetFileCapabilitiesV2(test_file, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());

  // Read it back.
  vfs_cap_data read_data{};
  ASSERT_THAT(getxattr(test_file.c_str(), "security.capability", &read_data, sizeof(read_data)),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(vfs_cap_data))));

  uint32_t expected_magic = VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE;
  EXPECT_EQ(le32toh(read_data.magic_etc), expected_magic);
  EXPECT_EQ(le32toh(read_data.data[0].permitted), static_cast<uint32_t>(caps));
  EXPECT_EQ(le32toh(read_data.data[0].inheritable), 0u);
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   Starting with Linux 4.14, a security.capability extended attribute is
//   automatically created as (or converted to) a version 3 (VFS_CAP_REVISION_3)
//   attribute if both of the following are true:
//
//   •  The thread writing the attribute resides in a noninitial user
//      namespace.
//   •  The thread has the CAP_SETFCAP capability over the file inode [...].
//
//   When a VFS_CAP_REVISION_3 security.capability extended attribute is
//   created, the root user ID of the creating thread's user namespace is saved
//   in the extended attribute.
//
//   Correspondingly, when a version 3 security.capability attribute is
//   retrieved (getxattr(2)) by a process that resides inside a user namespace
//   that was created by the root user ID (or a descendant of that user
//   namespace), the returned attribute is (automatically) simplified to appear
//   as a version 2 attribute (i.e., the returned value is the size of a
//   version 2 attribute and does not include the root user ID).
//
// Verifies V3-to-V2 coercion inside a matching user namespace, automatic
// conversion of V2 writes inside a non-initial user namespace to V3 on disk,
// and that reading a V3 attribute whose rootid cannot be mapped into the
// reader's user namespace fails with EOVERFLOW.
TEST_F(FileCapTest, FileCapabilityCoercionInsideUserNamespace) {
  if (!ProbeUserNamespaceSupport()) {
    GTEST_SKIP() << "User namespaces (CLONE_NEWUSER) not supported in this environment.";
  }

  uint64_t caps = (1ULL << CAP_NET_ADMIN);

  // Create the test files directly in the fixture's tmpfs mount path().
  // Since path() is chmod 0755, it is traversable by the child process.
  std::string file_v3_9999 = (path() / "file_v3_9999").string();
  std::string file_v3_8888 = (path() / "file_v3_8888").string();
  std::string file_written_in_ns = (path() / "file_written_in_ns").string();

  ASSERT_TRUE(files::WriteFile(file_v3_9999, ""));
  ASSERT_TRUE(files::WriteFile(file_v3_8888, ""));
  ASSERT_TRUE(files::WriteFile(file_written_in_ns, ""));

  ASSERT_THAT(SetFileCapabilitiesV3(file_v3_9999, caps, /*inheritable=*/0, /*effective=*/true,
                                    /*rootid=*/9999),
              SyscallSucceeds());
  ASSERT_THAT(SetFileCapabilitiesV3(file_v3_8888, caps, /*inheritable=*/0, /*effective=*/true,
                                    /*rootid=*/8888),
              SyscallSucceeds());

  // Make them accessible so the user in the child namespace can read/write them.
  ASSERT_THAT(chmod(file_v3_9999.c_str(), 0666), SyscallSucceeds());
  ASSERT_THAT(chmod(file_v3_8888.c_str(), 0666), SyscallSucceeds());
  ASSERT_THAT(chown(file_written_in_ns.c_str(), 9999, 9999), SyscallSucceeds());
  ASSERT_THAT(chmod(file_written_in_ns.c_str(), 0666), SyscallSucceeds());

  test_helper::ForkHelper helper;
  test_helper::Rendezvous child_unshared = test_helper::MakeRendezvous();
  test_helper::Rendezvous parent_mapped = test_helper::MakeRendezvous();

  pid_t pid = helper.RunInForkedProcess([&]() {
    // Child process: close the write end used by the parent.
    parent_mapped.poker = test_helper::Poker();

    // 1. Drop privileges to host UID/GID 9999.
    ASSERT_THAT(setresgid(9999, 9999, 9999), SyscallSucceeds());
    ASSERT_THAT(setresuid(9999, 9999, 9999), SyscallSucceeds());

    // 2. Unshare user namespace.
    ASSERT_THAT(unshare(CLONE_NEWUSER), SyscallSucceeds());

    // 3. Notify parent we have unshared.
    child_unshared.poker.poke();

    // 4. Wait for parent to write mappings.
    parent_mapped.holder.hold();

    // Now we are root (0) in the new namespace, which maps to 9999 on the host.
    EXPECT_EQ(getuid(), 0u);

    // 5. Perform getxattr / setxattr tests.

    // Test 1: file_v3_9999 (rootid = 9999).
    // Since 9999 maps to 0 (our root), this should be coerced to V2 (20 bytes).
    vfs_cap_data read_data_v2{};
    ASSERT_THAT(
        getxattr(file_v3_9999.c_str(), "security.capability", &read_data_v2, sizeof(read_data_v2)),
        SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(vfs_cap_data))));
    uint32_t expected_magic = VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE;
    EXPECT_EQ(le32toh(read_data_v2.magic_etc), expected_magic);

    // Test 2: file_v3_8888 (rootid = 8888).
    // Since 8888 (sibling) does NOT map to our root (9999), and is not an ancestor,
    // the kernel cannot map the rootid into our namespace. Therefore, getxattr
    // must FAIL with EOVERFLOW.
    vfs_ns_cap_data read_data_v3{};
    EXPECT_THAT(
        getxattr(file_v3_8888.c_str(), "security.capability", &read_data_v3, sizeof(read_data_v3)),
        SyscallFailsWithErrno(EOVERFLOW));

    // Test 3: Writing a V2 attribute inside the non-initial user namespace automatically
    // creates a V3 attribute on disk (which reads back as V2 inside this namespace).
    ASSERT_THAT(SetFileCapabilitiesV2(file_written_in_ns, caps, /*inheritable=*/0,
                                      /*effective=*/true),
                SyscallSucceeds());
    vfs_cap_data read_written_v2{};
    ASSERT_THAT(getxattr(file_written_in_ns.c_str(), "security.capability", &read_written_v2,
                         sizeof(read_written_v2)),
                SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(vfs_cap_data))));
    EXPECT_EQ(le32toh(read_written_v2.magic_etc), expected_magic);
  });

  // Parent process: close the write end used by the child.
  child_unshared.poker = test_helper::Poker();

  // 1. Wait for child to unshare.
  child_unshared.holder.hold();

  // 2. Write mappings for the child.
  std::string uid_map_path = "/proc/" + std::to_string(pid) + "/uid_map";
  std::string gid_map_path = "/proc/" + std::to_string(pid) + "/gid_map";

  ASSERT_TRUE(files::WriteFile(uid_map_path, "0 9999 1\n"));
  ASSERT_TRUE(files::WriteFile(gid_map_path, "0 9999 1\n"));

  // 3. Notify child that mappings are ready.
  parent_mapped.poker.poke();

  // 4. Wait for child to exit and check status.
  EXPECT_TRUE(helper.WaitForChildren());

  // 5. Verify from the initial user namespace that file_written_in_ns was stored as a
  // V3 attribute with rootid = 9999.
  vfs_ns_cap_data host_read_v3{};
  ASSERT_THAT(getxattr(file_written_in_ns.c_str(), "security.capability", &host_read_v3,
                       sizeof(host_read_v3)),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(vfs_ns_cap_data))));
  EXPECT_EQ(le32toh(host_read_v3.magic_etc),
            static_cast<uint32_t>(VFS_CAP_REVISION_3 | VFS_CAP_FLAGS_EFFECTIVE));
  EXPECT_EQ(le32toh(host_read_v3.rootid), 9999u);
}

// From <https://man7.org/linux/man-pages/man2/chown.2.html>:
//
//   When the owner or group of an executable file is changed (by any user),
//   all capability sets for the file are cleared.
//
//   If the owner or group is specified as -1, then that ID is not
//   changed.
//
// From <https://man7.org/linux/man-pages/man3/write.3p.html>:
//
//   If the file is a regular file, the S_ISUID and S_ISGID bits of the file
//   mode may be cleared.
//
// From <https://man7.org/linux/man-pages/man2/truncate.2.html>:
//
//   If the size changed, then [...] the set-user-ID and set-group-ID mode bits
//   may be cleared.
//
// Verifies that modifying a file via write(2), truncate(2), ftruncate(2), or
// open(O_TRUNC), or calling chown(2) or fchown(2), removes any
// security.capability extended attribute from the file.
struct FileCapClearParam {
  std::string name;
  std::function<void(const std::filesystem::path &path)> operation;
};

class FileCapClearTest : public FileCapTest,
                         public ::testing::WithParamInterface<FileCapClearParam> {};

TEST_P(FileCapClearTest, ClearsFileCapabilities) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  std::filesystem::path test_file = path() / "test_file";
  ASSERT_TRUE(files::WriteFile(test_file.string(), "initial content"));

  ASSERT_THAT(SetFileCapabilitiesV2(test_file, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());
  vfs_cap_data read_data{};
  ASSERT_THAT(getxattr(test_file.c_str(), "security.capability", &read_data, sizeof(read_data)),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(vfs_cap_data))));

  GetParam().operation(test_file);

  EXPECT_THAT(getxattr(test_file.c_str(), "security.capability", &read_data, sizeof(read_data)),
              SyscallFailsWithErrno(ENODATA));
}

INSTANTIATE_TEST_SUITE_P(
    WriteTruncateAndChown, FileCapClearTest,
    ::testing::Values(
        FileCapClearParam{
            .name = "Write",
            .operation =
                [](const std::filesystem::path &path) {
                  fbl::unique_fd fd(open(path.c_str(), O_WRONLY));
                  ASSERT_TRUE(fd.is_valid());
                  ASSERT_THAT(write(fd.get(), "x", 1), SyscallSucceedsWithValue(1));
                },
        },
        FileCapClearParam{
            .name = "Truncate",
            .operation =
                [](const std::filesystem::path &path) {
                  ASSERT_THAT(truncate(path.c_str(), 4), SyscallSucceeds());
                },
        },
        FileCapClearParam{
            .name = "Ftruncate",
            .operation =
                [](const std::filesystem::path &path) {
                  fbl::unique_fd fd(open(path.c_str(), O_WRONLY));
                  ASSERT_TRUE(fd.is_valid());
                  ASSERT_THAT(ftruncate(fd.get(), 2), SyscallSucceeds());
                },
        },
        FileCapClearParam{
            .name = "OpenTrunc",
            .operation =
                [](const std::filesystem::path &path) {
                  fbl::unique_fd trunc_fd(open(path.c_str(), O_WRONLY | O_TRUNC));
                  ASSERT_TRUE(trunc_fd.is_valid());
                },
        },
        FileCapClearParam{
            .name = "Chown",
            .operation =
                [](const std::filesystem::path &path) {
                  ASSERT_THAT(chown(path.c_str(), kUser1Uid, kUser1Gid), SyscallSucceeds());
                },
        },
        FileCapClearParam{
            .name = "Fchown",
            .operation =
                [](const std::filesystem::path &path) {
                  fbl::unique_fd fd(open(path.c_str(), O_WRONLY));
                  ASSERT_TRUE(fd.is_valid());
                  ASSERT_THAT(fchown(fd.get(), kRootUid, kRootGid), SyscallSucceeds());
                },
        },
        FileCapClearParam{
            .name = "ChownMinusOne",
            .operation =
                [](const std::filesystem::path &path) {
                  test_helper::ForkHelper helper;
                  helper.RunInForkedProcess([&]() {
                    SwitchToUser1();
                    ASSERT_THAT(chown(path.c_str(), -1, -1), SyscallSucceeds());
                  });
                  ASSERT_TRUE(helper.WaitForChildren());
                },
        }),
    [](const ::testing::TestParamInfo<FileCapClearParam> &info) { return info.param.name; });

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   The file capability sets are stored in an extended attribute (see
//   setxattr(2) and xattr(7)) named security.capability.  Writing to this
//   extended attribute requires the CAP_SETFCAP capability.
//
// Verifies that calling setxattr(2) on security.capability without CAP_SETFCAP
// fails with EPERM, even if the caller owns the file and is attempting to set
// an empty capability mask.
TEST_F(FileCapTest, SetFileCapabilitiesWithoutCapSetfcapFails) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  std::string test_file = (path() / "test_file").string();
  ASSERT_TRUE(files::WriteFile(test_file, ""));
  ASSERT_THAT(chown(test_file.c_str(), kUser1Uid, kUser1Gid), SyscallSucceeds());
  ASSERT_THAT(chmod(test_file.c_str(), 0644), SyscallSucceeds());
  ASSERT_THAT(SetFileCapabilitiesV2(test_file, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&]() {
    SwitchToUser1();

    vfs_cap_data data{};
    data.magic_etc = htole32(VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE);
    EXPECT_THAT(setxattr(test_file.c_str(), "security.capability", &data, sizeof(data), 0),
                SyscallFailsWithErrno(EPERM));
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

// From <https://man7.org/linux/man-pages/man7/xattr.7.html>:
//
//   Extended security attributes
//          Read and write access permissions to security attributes depend on
//          the policy implemented for each security attribute by the security
//          module.  When no security module is loaded, all processes have read
//          access to extended security attributes, and write access is limited
//          to processes that have the CAP_SYS_ADMIN capability.
//
// Verifies that removing security.capability via removexattr(2) requires the
// CAP_SETFCAP capability (failing with EPERM without it, and succeeding with
// it).
TEST_F(FileCapTest, RemoveFileCapabilitiesRequiresCapSetfcap) {
  uint64_t caps = (1ULL << CAP_NET_ADMIN);
  std::string test_file = (path() / "test_file").string();
  ASSERT_TRUE(files::WriteFile(test_file, ""));
  ASSERT_THAT(chown(test_file.c_str(), kUser1Uid, kUser1Gid), SyscallSucceeds());
  ASSERT_THAT(chmod(test_file.c_str(), 0644), SyscallSucceeds());
  ASSERT_THAT(SetFileCapabilitiesV2(test_file, caps, /*inheritable=*/0, /*effective=*/true),
              SyscallSucceeds());

  // 1. Without CAP_SETFCAP (even as file owner, or as root without CAP_SETFCAP),
  // removexattr(2) on security.capability fails with EPERM.
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&]() {
    SwitchToUser1();
    EXPECT_THAT(removexattr(test_file.c_str(), "security.capability"),
                SyscallFailsWithErrno(EPERM));
  });
  EXPECT_TRUE(helper.WaitForChildren());

  helper.RunInForkedProcess([&]() {
    test_helper::UnsetCapabilityEffective(CAP_SETFCAP);
    EXPECT_THAT(removexattr(test_file.c_str(), "security.capability"),
                SyscallFailsWithErrno(EPERM));
  });
  EXPECT_TRUE(helper.WaitForChildren());

  vfs_cap_data read_data{};
  EXPECT_THAT(getxattr(test_file.c_str(), "security.capability", &read_data, sizeof(read_data)),
              SyscallSucceedsWithValue(static_cast<ssize_t>(sizeof(vfs_cap_data))));

  // 2. With CAP_SETFCAP, removexattr(2) succeeds and removes the attribute.
  EXPECT_THAT(removexattr(test_file.c_str(), "security.capability"), SyscallSucceeds());
  EXPECT_THAT(getxattr(test_file.c_str(), "security.capability", &read_data, sizeof(read_data)),
              SyscallFailsWithErrno(ENODATA));
}

// From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
//
//   The kernel transparently continues to support the execution of files that
//   have 32-bit version 1 capability masks, but when adding capabilities to
//   files that did not previously have capabilities, or modifying the
//   capabilities of existing files, it automatically uses the version 2 scheme
//   (or possibly the version 3 scheme, as described below).
//
// Writing VFS_CAP_REVISION_1 payloads (12 bytes), truncated payloads, unknown
// revisions, or unknown flag bits to `security.capability` fails with EINVAL.
TEST_F(FileCapTest, MalformedAndRevision1FileCapabilitiesRejected) {
  std::string test_file = (path() / "test_file").string();
  ASSERT_TRUE(files::WriteFile(test_file, ""));

  // 1. VFS_CAP_REVISION_1 (12 bytes) is rejected.
  struct vfs_cap_data_v1 {
    uint32_t magic_etc;
    uint32_t permitted;
    uint32_t inheritable;
  } data_v1{};
  data_v1.magic_etc = htole32(VFS_CAP_REVISION_1 | VFS_CAP_FLAGS_EFFECTIVE);
  EXPECT_THAT(setxattr(test_file.c_str(), "security.capability", &data_v1, sizeof(data_v1), 0),
              SyscallFailsWithErrno(EINVAL));

  // 2. Truncated payload lengths (e.g. 4 bytes) are rejected.
  uint32_t short_buf = htole32(VFS_CAP_REVISION_2);
  EXPECT_THAT(setxattr(test_file.c_str(), "security.capability", &short_buf, sizeof(short_buf), 0),
              SyscallFailsWithErrno(EINVAL));

  // 3. Unknown revision number (20 bytes) is rejected.
  vfs_cap_data unknown_rev{};
  unknown_rev.magic_etc = htole32(0x04000000);
  EXPECT_THAT(
      setxattr(test_file.c_str(), "security.capability", &unknown_rev, sizeof(unknown_rev), 0),
      SyscallFailsWithErrno(EINVAL));

  // 4. Reserved/unknown flag bits in magic_etc are rejected.
  vfs_cap_data bad_flags{};
  bad_flags.magic_etc = htole32(VFS_CAP_REVISION_2 | 0x00000002);
  EXPECT_THAT(setxattr(test_file.c_str(), "security.capability", &bad_flags, sizeof(bad_flags), 0),
              SyscallFailsWithErrno(EINVAL));

  // 5. Mismatched revision and payload size (V2 magic with 24-byte buffer, V3 magic with 20-byte
  // buffer) are rejected.
  vfs_ns_cap_data v2_in_v3_buf{};
  v2_in_v3_buf.magic_etc = htole32(VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE);
  EXPECT_THAT(
      setxattr(test_file.c_str(), "security.capability", &v2_in_v3_buf, sizeof(v2_in_v3_buf), 0),
      SyscallFailsWithErrno(EINVAL));

  vfs_cap_data v3_in_v2_buf{};
  v3_in_v2_buf.magic_etc = htole32(VFS_CAP_REVISION_3 | VFS_CAP_FLAGS_EFFECTIVE);
  EXPECT_THAT(
      setxattr(test_file.c_str(), "security.capability", &v3_in_v2_buf, sizeof(v3_in_v2_buf), 0),
      SyscallFailsWithErrno(EINVAL));
}
