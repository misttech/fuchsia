// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/stdcompat/string_view.h>
#include <mntent.h>
#include <sys/mount.h>
#include <unistd.h>

#include <gtest/gtest.h>

#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

extern std::string DoPrePolicyLoadWork() { return "minimal_policy.pp"; }

namespace {

std::string MountOptionsFor(std::string_view mount_path) {
  FILE* mounts = setmntent("/proc/mounts", "r");
  std::string result;
  for (struct mntent* entry = 0; (entry = getmntent(mounts));) {
    if (mount_path == entry->mnt_dir) {
      result = entry->mnt_opts;
      break;
    }
  }
  endmntent(mounts);
  return result;
}

TEST(MountTest, NoSelinuxMountOptions) {
  ASSERT_THAT(mkdir("/mount_tests", 0755), SyscallSucceeds());
  ASSERT_THAT(mount("tmpfs", "/mount_tests", "tmpfs", MS_NOEXEC | MS_NOSUID | MS_NODEV, 0),
              SyscallSucceeds());

  std::string mount_options = MountOptionsFor("/mount_tests");
  ASSERT_THAT(umount("/mount_tests"), SyscallSucceeds());
  ASSERT_THAT(rmdir("/mount_tests"), SyscallSucceeds());

  EXPECT_TRUE(cpp23::contains(mount_options, "nosuid"));
  EXPECT_TRUE(cpp23::contains(mount_options, "noexec"));
  EXPECT_TRUE(cpp23::contains(mount_options, "nodev"));
}

TEST(MountTest, WithContextOption) {
  ASSERT_THAT(mkdir("/mount_tests", 0755), SyscallSucceeds());
  ASSERT_THAT(mount("tmpfs", "/mount_tests", "tmpfs", MS_NOEXEC | MS_NOSUID | MS_NODEV,
                    "context=source_u:object_r:target_t:s0"),
              SyscallSucceeds());

  std::string mount_options = MountOptionsFor("/mount_tests");
  ASSERT_THAT(umount("/mount_tests"), SyscallSucceeds());
  ASSERT_THAT(rmdir("/mount_tests"), SyscallSucceeds());

  EXPECT_FALSE(cpp23::contains(mount_options, "seclabel"));
  EXPECT_TRUE(cpp23::contains(mount_options, "context="));
}

TEST(MountTest, WithSeclabel) {
  // Base policy uses "fs_use_trans" labeling scheme for "tmpfs", which should report "seclabel".
  ASSERT_THAT(mkdir("/with_seclabel", 0755), SyscallSucceeds());
  ASSERT_THAT(mount("tmpfs", "/with_seclabel", "tmpfs", MS_NOEXEC | MS_NOSUID | MS_NODEV, 0),
              SyscallSucceeds());

  std::string mount_options = MountOptionsFor("/with_seclabel");
  ASSERT_THAT(umount("/with_seclabel"), SyscallSucceeds());
  ASSERT_THAT(rmdir("/with_seclabel"), SyscallSucceeds());

  EXPECT_TRUE(cpp23::contains(mount_options, "seclabel"));
}

TEST(MountTest, WithoutSeclabel) {
  // Base policy uses "genfscon" labeling scheme for "proc", which should not report "seclabel".
  ASSERT_THAT(mkdir("/without_seclabel", 0755), SyscallSucceeds());
  ASSERT_THAT(
      mount("selinuxfs", "/without_seclabel", "selinuxfs", MS_NOEXEC | MS_NOSUID | MS_NODEV, 0),
      SyscallSucceeds());

  std::string mount_options = MountOptionsFor("/without_seclabel");
  ASSERT_THAT(umount("/without_seclabel"), SyscallSucceeds());
  ASSERT_THAT(rmdir("/without_seclabel"), SyscallSucceeds());

  EXPECT_FALSE(cpp23::contains(mount_options, "seclabel"));
}

}  // namespace
