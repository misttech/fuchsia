// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <unistd.h>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

extern std::string DoPrePolicyLoadWork() { return "inherit_policy"; }

namespace {

struct FcntlTestParam {
  int cmd;
  std::string name;
  unsigned long arg;
  bool expect_success_without_fd_use;
};

class FcntlTest : public testing::TestWithParam<FcntlTestParam> {};

TEST_P(FcntlTest, WithFdUse) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  const auto param = FcntlTest::GetParam();

  // Create a temporary file within the parent domain.
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_inherit_parent_t:s0", [&] {
    fbl::unique_fd fd(open("/tmp", O_RDWR | O_TMPFILE, 0600));
    ASSERT_TRUE(fd.is_valid()) << "Failed to create tmpfile: " << strerror(errno);

    // Domain granted FD-use should always have access.
    ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_inherit_child_allow_use_fd_t:s0", [&] {
      EXPECT_THAT(fcntl(fd.get(), param.cmd, param.arg), SyscallSucceeds());
    }));
  }));
}

TEST_P(FcntlTest, WithoutFdUse) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  const auto param = FcntlTest::GetParam();

  // Create a temporary file within the parent domain.
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_inherit_parent_t:s0", [&] {
    fbl::unique_fd fd(open("/tmp", O_RDWR | O_TMPFILE, 0600));
    ASSERT_TRUE(fd.is_valid()) << "Failed to create tmpfile: " << strerror(errno);

    // Domain not granted FD-use.
    ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_inherit_child_no_use_fd_t:s0", [&] {
      if (param.expect_success_without_fd_use) {
        EXPECT_THAT(fcntl(fd.get(), param.cmd, param.arg), SyscallSucceeds());
      } else {
        EXPECT_THAT(fcntl(fd.get(), param.cmd, param.arg), SyscallFailsWithErrno(EACCES));
      }
    }));
  }));
}

INSTANTIATE_TEST_SUITE_P(Fcntl, FcntlTest,
                         ::testing::Values(FcntlTestParam{F_GETFD, "F_GETFD", 0, true},
                                           FcntlTestParam{F_GETFL, "F_GETFL", 0, false},
                                           FcntlTestParam{F_GETOWN, "F_GETOWN", 0, true},
                                           FcntlTestParam{F_SETFD, "F_SETFD", FD_CLOEXEC, true},
                                           FcntlTestParam{F_SETFL, "F_SETFL", O_NONBLOCK, false}),
                         [](const testing::TestParamInfo<FcntlTestParam>& info) {
                           return info.param.name;
                         });

}  // namespace
