// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <lib/stdcompat/string_view.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/xattr.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/base_initial_sids.h"
#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

constexpr char kPrePolicyMountDir[] = "/tmp/pre_policy_ramfs";
constexpr char kPrePolicyFilePath[] = "/tmp/pre_policy_ramfs/pre_existing_file";

}  // namespace

extern std::string DoPrePolicyLoadWork() {
  EXPECT_THAT(mkdir(kPrePolicyMountDir, 0755), SyscallSucceeds());
  EXPECT_THAT(mount("none", kPrePolicyMountDir, "ramfs", 0, ""), SyscallSucceeds());
  fbl::unique_fd fd(open(kPrePolicyFilePath, O_CREAT | O_RDWR, 0644));
  EXPECT_TRUE(fd.is_valid()) << strerror(errno);
  return "file_transition_policy";
}

namespace {

TEST(UnlabeledFsTest, PreExistingFileInUnlabeledFs) {
  EXPECT_EQ(GetLabel(kPrePolicyMountDir), kUnlabeledInitialSid);
  EXPECT_EQ(GetLabel(kPrePolicyFilePath), kUnlabeledInitialSid);
}

TEST(UnlabeledFsTest, CreateFileInUnlabeledFs) {
  test_helper::ScopedTempDir temp_dir;
  auto mount = ASSERT_RESULT_SUCCESS_AND_RETURN(
      test_helper::ScopedMount::Mount("none", temp_dir.path(), "ramfs", 0, ""));

  // Check that `ramfs`, which has no labeling rules in the policy, is labeled with the `unlabeled`
  // initial SID.
  EXPECT_EQ(GetLabel(temp_dir.path()), kUnlabeledInitialSid);

  ASSERT_TRUE(RunSubprocessAs("unconfined_u:unconfined_r:unconfined_t:s0", [&] {
    // Check that creating a file in an unlabeled filesystem follows transition rules.
    std::string file_path = temp_dir.path() + "/test-file";
    auto fd = fbl::unique_fd(open(file_path.c_str(), O_CREAT, 0777));
    ASSERT_THAT(fd.get(), SyscallSucceeds()) << "while creating file";
    EXPECT_EQ(GetLabel(fd.get()), "unconfined_u:object_r:unlabeled_unconfined_file_t:s0");
  }));
}

TEST(UnlabeledFsTest, IgnoresFscreate) {
  test_helper::ScopedTempDir temp_dir;
  auto mount = ASSERT_RESULT_SUCCESS_AND_RETURN(
      test_helper::ScopedMount::Mount("none", temp_dir.path(), "ramfs", 0, ""));

  ASSERT_TRUE(RunSubprocessAs("unconfined_u:unconfined_r:unconfined_t:s0", [&] {
    ASSERT_THAT(WriteTaskAttr("fscreate", "unconfined_u:object_r:unconfined_t:s0"),
                SyscallResultIsOk());

    std::string file_path = temp_dir.path() + "/test-file";
    auto fd = fbl::unique_fd(open(file_path.c_str(), O_CREAT, 0777));
    ASSERT_THAT(fd.get(), SyscallSucceeds()) << "while creating file";
    EXPECT_EQ(GetLabel(fd.get()), "unconfined_u:object_r:unlabeled_unconfined_file_t:s0");
  }));
}

TEST(UnlabeledFsTest, MountWithFsContextAndRootContext) {
  {
    test_helper::ScopedTempDir temp_dir;
    auto mount = ASSERT_RESULT_SUCCESS_AND_RETURN(test_helper::ScopedMount::Mount(
        "none", temp_dir.path(), "ramfs", 0, "fscontext=unconfined_u:object_r:unconfined_t:s0"));
    EXPECT_EQ(GetLabel(temp_dir.path()), "unconfined_u:object_r:unconfined_t:s0");
  }

  {
    test_helper::ScopedTempDir temp_dir;
    auto mount = ASSERT_RESULT_SUCCESS_AND_RETURN(test_helper::ScopedMount::Mount(
        "none", temp_dir.path(), "ramfs", 0, "rootcontext=unconfined_u:object_r:unconfined_t:s0"));
    EXPECT_EQ(GetLabel(temp_dir.path()), "unconfined_u:object_r:unconfined_t:s0");
  }

  {
    test_helper::ScopedTempDir temp_dir;
    auto mount = ASSERT_RESULT_SUCCESS_AND_RETURN(test_helper::ScopedMount::Mount(
        "none", temp_dir.path(), "ramfs", 0,
        "fscontext=unconfined_u:object_r:tmpfs_t:s0,rootcontext=unconfined_u:object_r:unconfined_t:s0"));
    EXPECT_EQ(GetLabel(temp_dir.path()), "unconfined_u:object_r:unconfined_t:s0");
  }
}

TEST(UnlabeledFsTest, DoesNotSupportRelabeling) {
  test_helper::ScopedTempDir temp_dir;
  auto mount = ASSERT_RESULT_SUCCESS_AND_RETURN(
      test_helper::ScopedMount::Mount("none", temp_dir.path(), "ramfs", 0, ""));

  EXPECT_FALSE(cpp23::contains(MountOptionsFor(temp_dir.path()), "seclabel"));
  EXPECT_THAT(SetLabel(temp_dir.path(), "unconfined_u:object_r:unconfined_t:s0"),
              SyscallResultIsErrno(ENOTSUP));
}

}  // namespace
