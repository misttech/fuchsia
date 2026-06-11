// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

extern std::string DoPrePolicyLoadWork() { return "fs_test_policy"; }

namespace {
constexpr char kDirLabel[] = "test_u:object_r:test_fs_readdir_dir_t:s0";

TEST(FsTest, ReaddirAllowed) {
  // Create a directory with the specific label.
  auto fscreate = ScopedTaskAttrResetter::SetTaskAttr("fscreate", kDirLabel);
  test_helper::ScopedTempDir temp_dir;

  // Open the directory.
  fbl::unique_fd unique_fd(open(temp_dir.path().c_str(), O_RDONLY | O_DIRECTORY));
  ASSERT_THAT(unique_fd.get(), SyscallSucceeds());

  auto enforcing = ScopedEnforcement::SetEnforcing();

  // Run as a domain that is allowed to read the directory.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_fs_t:s0", [&]() {
    char buf[1024];
    EXPECT_THAT(syscall(SYS_getdents64, unique_fd.get(), buf, sizeof(buf)), SyscallSucceeds());
  }));
}

TEST(FsTest, ReaddirDenied) {
  // Create a directory with the specific label.
  auto fscreate = ScopedTaskAttrResetter::SetTaskAttr("fscreate", kDirLabel);
  test_helper::ScopedTempDir temp_dir;

  // Open the directory.
  fbl::unique_fd unique_fd(open(temp_dir.path().c_str(), O_RDONLY | O_DIRECTORY));
  ASSERT_THAT(unique_fd.get(), SyscallSucceeds());

  auto enforcing = ScopedEnforcement::SetEnforcing();

  // Run as a domain that is NOT allowed to read the directory.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_fs_no_read_t:s0", [&]() {
    char buf[1024];
    EXPECT_THAT(syscall(SYS_getdents64, unique_fd.get(), buf, sizeof(buf)),
                SyscallFailsWithErrno(EACCES));
  }));
}

constexpr char kFallocateFileLabel[] = "test_u:object_r:test_fs_fallocate_file_t:s0";

// Verify that fallocate succeeds for a domain with write permission.
TEST(FsTest, FallocateAllowed) {
  auto test_file = ScopedTempFDWithLabel(kFallocateFileLabel);
  ASSERT_TRUE(test_file.is_valid());
  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_fs_t:s0", [&]() {
    EXPECT_THAT(fallocate(test_file.fd(), 0, 0, 1024), SyscallSucceeds());
  }));
}

// Verify that fallocate fails for a domain without write permission.
TEST(FsTest, FallocateDenied) {
  auto test_file = ScopedTempFDWithLabel(kFallocateFileLabel);
  ASSERT_TRUE(test_file.is_valid());
  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_fs_no_write_t:s0", [&]() {
    EXPECT_THAT(fallocate(test_file.fd(), 0, 0, 1024), SyscallFailsWithErrno(EACCES));
  }));
}

}  // namespace
