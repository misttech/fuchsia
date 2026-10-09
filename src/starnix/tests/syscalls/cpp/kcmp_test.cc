// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/kcmp.h>

#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

TEST(KcmpTest, SameProcessResources) {
  pid_t pid = getpid();
  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_VM, 0, 0), SyscallSucceedsWithValue(0));
  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_FILES, 0, 0), SyscallSucceedsWithValue(0));
  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_FS, 0, 0), SyscallSucceedsWithValue(0));
  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_SIGHAND, 0, 0), SyscallSucceedsWithValue(0));

  fbl::unique_fd fd1(open("/dev/null", O_RDONLY));
  ASSERT_TRUE(fd1.is_valid()) << strerror(errno);
  fbl::unique_fd fd2(dup(fd1.get()));
  ASSERT_TRUE(fd2.is_valid()) << strerror(errno);
  fbl::unique_fd fd3(open("/dev/null", O_RDONLY));
  ASSERT_TRUE(fd3.is_valid()) << strerror(errno);

  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_FILE, fd1.get(), fd2.get()),
              SyscallSucceedsWithValue(0));
  long cmp_1_3 = syscall(SYS_kcmp, pid, pid, KCMP_FILE, fd1.get(), fd3.get());
  ASSERT_TRUE(cmp_1_3 == 1 || cmp_1_3 == 2) << "cmp_1_3=" << cmp_1_3;
  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_FILE, fd3.get(), fd1.get()),
              SyscallSucceedsWithValue(cmp_1_3 == 1 ? 2 : 1));
  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_FILE, -1, fd2.get()), SyscallFailsWithErrno(EBADF));
  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_FILE, fd1.get(), -1), SyscallFailsWithErrno(EBADF));
}

TEST(KcmpTest, SysvsemUnsupported) {
  pid_t pid = getpid();
  if (!test_helper::IsStarnix() && syscall(SYS_kcmp, pid, pid, KCMP_SYSVSEM, 0, 0) == 0) {
    GTEST_SKIP() << "System V IPC is enabled on this kernel";
  }

  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_SYSVSEM, 0, 0), SyscallFailsWithErrno(EOPNOTSUPP));
  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_SYSVSEM, 1, 2), SyscallFailsWithErrno(EOPNOTSUPP));
  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_SYSVSEM, ULONG_MAX, ULONG_MAX),
              SyscallFailsWithErrno(EOPNOTSUPP));

  test_helper::ForkHelper helper;
  pid_t child_pid = helper.RunInForkedProcess([] { pause(); });
  EXPECT_THAT(syscall(SYS_kcmp, pid, child_pid, KCMP_SYSVSEM, 0, 0),
              SyscallFailsWithErrno(EOPNOTSUPP));
  EXPECT_THAT(syscall(SYS_kcmp, child_pid, pid, KCMP_SYSVSEM, 0, 0),
              SyscallFailsWithErrno(EOPNOTSUPP));
  ASSERT_EQ(0, kill(child_pid, SIGKILL)) << strerror(errno);
  helper.ExpectSignal(SIGKILL);
  ASSERT_TRUE(helper.WaitForChildren());
}

TEST(KcmpTest, InvalidResourceType) {
  pid_t pid = getpid();
  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_TYPES, 0, 0), SyscallFailsWithErrno(EINVAL));
  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, KCMP_TYPES + 1, 0, 0), SyscallFailsWithErrno(EINVAL));
  EXPECT_THAT(syscall(SYS_kcmp, pid, pid, UINT_MAX, 0, 0), SyscallFailsWithErrno(EINVAL));
}

TEST(KcmpTest, NonExistentProcess) {
  pid_t pid = getpid();
  EXPECT_THAT(syscall(SYS_kcmp, 0, pid, KCMP_SYSVSEM, 0, 0), SyscallFailsWithErrno(ESRCH));
  EXPECT_THAT(syscall(SYS_kcmp, pid, 0, KCMP_SYSVSEM, 0, 0), SyscallFailsWithErrno(ESRCH));
  EXPECT_THAT(syscall(SYS_kcmp, -1, pid, KCMP_SYSVSEM, 0, 0), SyscallFailsWithErrno(ESRCH));
  EXPECT_THAT(syscall(SYS_kcmp, pid, -1, KCMP_SYSVSEM, 0, 0), SyscallFailsWithErrno(ESRCH));
}

TEST(KcmpTest, UnprivilegedProcessPermissionDenied) {
  pid_t parent_pid = getpid();
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([parent_pid] {
    if (setresuid(1, 1, 1) == 0) {
      pid_t child_pid = getpid();
      EXPECT_THAT(syscall(SYS_kcmp, parent_pid, parent_pid, KCMP_SYSVSEM, 0, 0),
                  SyscallFailsWithErrno(EPERM));
      EXPECT_THAT(syscall(SYS_kcmp, child_pid, parent_pid, KCMP_SYSVSEM, 0, 0),
                  SyscallFailsWithErrno(EPERM));
      EXPECT_THAT(syscall(SYS_kcmp, parent_pid, child_pid, KCMP_SYSVSEM, 0, 0),
                  SyscallFailsWithErrno(EPERM));
    } else if (kill(1, 0) == -1 && errno == EPERM) {
      EXPECT_THAT(syscall(SYS_kcmp, parent_pid, 1, KCMP_SYSVSEM, 0, 0),
                  SyscallFailsWithErrno(EPERM));
      EXPECT_THAT(syscall(SYS_kcmp, 1, parent_pid, KCMP_SYSVSEM, 0, 0),
                  SyscallFailsWithErrno(EPERM));
    }
  });
  ASSERT_TRUE(helper.WaitForChildren());
}

}  // namespace
