// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <lib/stdcompat/string_view.h>
#include <mntent.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <map>
#include <string>

#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

std::map<std::string, fit::result<int>> g_pre_policy_mount_results;

const char* kSelinuxMountOptions[] = {"context", "fscontext", "defcontext", "rootcontext"};

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

class MountOptionValidationTest : public ::testing::TestWithParam<const char*> {};

TEST_P(MountOptionValidationTest, RejectedBeforePolicyLoad) {
  const char* option_name = GetParam();
  auto it = g_pre_policy_mount_results.find(option_name);
  ASSERT_NE(it, g_pre_policy_mount_results.end()) << "Result for " << option_name << " not found";
  const auto& result = it->second;

  EXPECT_THAT(result, SyscallResultIsErrno(EINVAL))
      << "Mount with '" << option_name << "' unexpectedly succeeded or failed with wrong error";
}

TEST_P(MountOptionValidationTest, RejectedWhenInvalid) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping suite.";
  }

  const char* option_name = GetParam();
  test_helper::ScopedTempDir mount_dir;

  // Providing a security context that is not valid in the loaded policy should result in EINVAL.
  std::string options = std::string(option_name) + "=test_u:object_r:invalid_context_t:s0";
  EXPECT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, options.c_str()),
              SyscallFailsWithErrno(EINVAL));
}

INSTANTIATE_TEST_SUITE_P(MountTests, MountOptionValidationTest,
                         ::testing::ValuesIn(kSelinuxMountOptions),
                         [](const testing::TestParamInfo<const char*>& info) {
                           return info.param;
                         });

TEST(MountTest, NoSelinuxMountOptions) {
  test_helper::ScopedTempDir mount_dir;
  ASSERT_THAT(
      mount("tmpfs", mount_dir.path().c_str(), "tmpfs", MS_NOEXEC | MS_NOSUID | MS_NODEV, 0),
      SyscallSucceeds());

  std::string mount_options = MountOptionsFor(mount_dir.path());

  EXPECT_TRUE(cpp23::contains(mount_options, "nosuid"));
  EXPECT_TRUE(cpp23::contains(mount_options, "noexec"));
  EXPECT_TRUE(cpp23::contains(mount_options, "nodev"));
}

TEST(MountTest, WithContextOption) {
  test_helper::ScopedTempDir mount_dir;
  ASSERT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", MS_NOEXEC | MS_NOSUID | MS_NODEV,
                    "context=test_u:object_r:test_mount_fscontext_t:s0"),
              SyscallSucceeds());

  std::string mount_options = MountOptionsFor(mount_dir.path());

  EXPECT_FALSE(cpp23::contains(mount_options, "seclabel"));
  EXPECT_TRUE(cpp23::contains(mount_options, "context="));
}

TEST(MountTest, WithSeclabel) {
  // Base policy uses "fs_use_trans" labeling scheme for "tmpfs", which should report "seclabel".
  test_helper::ScopedTempDir mount_dir;
  ASSERT_THAT(
      mount("tmpfs", mount_dir.path().c_str(), "tmpfs", MS_NOEXEC | MS_NOSUID | MS_NODEV, 0),
      SyscallSucceeds());

  std::string mount_options = MountOptionsFor(mount_dir.path());

  EXPECT_TRUE(cpp23::contains(mount_options, "seclabel"));
}

TEST(MountTest, WithoutSeclabel) {
  // Base policy uses "genfscon" labeling scheme for "proc", which should not report "seclabel".
  test_helper::ScopedTempDir mount_dir;
  ASSERT_THAT(mount("selinuxfs", mount_dir.path().c_str(), "selinuxfs",
                    MS_NOEXEC | MS_NOSUID | MS_NODEV, 0),
              SyscallSucceeds());

  std::string mount_options = MountOptionsFor(mount_dir.path());

  EXPECT_FALSE(cpp23::contains(mount_options, "seclabel"));
}

// Verifies that the relabelfrom and relabelto checks are applied when mounting a filesystem and
// overriding the policy default security label for that filesystem type.
TEST(MountTest, FsContextRequiresRelabelFromAndTo) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping suite.";
  }

  const char* fscontext = "fscontext=test_u:object_r:test_mount_fscontext_t:s0";
  test_helper::ScopedTempDir mount_dir;

  auto enforce = ScopedEnforcement::SetEnforcing();

  // 1. Verify that mounting fails when 'relabelto' is denied.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_relabelto_denied_t:s0", [&] {
    EXPECT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, fscontext),
                SyscallFailsWithErrno(EACCES));
  }));

  // 2. Verify that mounting fails when 'relabelfrom' is denied.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_relabelfrom_denied_t:s0", [&] {
    EXPECT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, fscontext),
                SyscallFailsWithErrno(EACCES));
  }));

  // 3. Verify that mounting succeeds when both 'relabelfrom' and 'relabelto' are allowed.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_relabel_allowed_t:s0", [&] {
    EXPECT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, fscontext), SyscallSucceeds());
  }));
}

// Verifies that the relabelfrom and relabelto checks are applied even when "fscontext=" names the
// label that the filesystem would have been given anyway.
TEST(MountTest, FsContextToSameLabelStillRequiresRelabel) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping suite.";
  }

  // "tmpfs_t" is the label the policy already assigns to "tmpfs" mounts, so this relabels the
  // filesystem to itself.
  const char* fscontext = "fscontext=system_u:object_r:tmpfs_t:s0";
  test_helper::ScopedTempDir mount_dir;

  auto enforce = ScopedEnforcement::SetEnforcing();

  // 1. Verify that mounting fails when 'relabelfrom' is denied.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_relabelfrom_denied_t:s0", [&] {
    EXPECT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, fscontext),
                SyscallFailsWithErrno(EACCES));
  }));

  // 2. Verify that mounting fails when 'relabelto' is denied.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_relabelto_denied_t:s0", [&] {
    EXPECT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, fscontext),
                SyscallFailsWithErrno(EACCES));
  }));

  // 3. Verify that mounting succeeds when both 'relabelfrom' and 'relabelto' are allowed.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_relabel_self_allowed_t:s0", [&] {
    EXPECT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, fscontext), SyscallSucceeds());
  }));
}

// Verifies that overriding the label of the root node with "rootcontext=" is gated on the caller
// having "relabelfrom" to the filesystem, and on the overriding label being allowed to "associate"
// with it. "rootcontext=" does not change the filesystem's own label, so no "relabelto" to the
// filesystem is required, and the checks are made against the "filesystem" class rather than the
// class of the node being labeled.
TEST(MountTest, RootContextRequiresRelabelFromAndAssociate) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping suite.";
  }

  const char* rootcontext = "rootcontext=test_u:object_r:test_mount_rootcontext_t:s0";
  test_helper::ScopedTempDir mount_dir;

  auto enforce = ScopedEnforcement::SetEnforcing();

  // 1. Verify that mounting fails when 'relabelfrom' to the filesystem is denied, even though
  //    "rootcontext=" leaves the filesystem's own label untouched.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_relabelfrom_denied_t:s0", [&] {
    EXPECT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, rootcontext),
                SyscallFailsWithErrno(EACCES));
  }));

  // 2. Verify that mounting fails when the overriding label is not allowed to 'associate' with
  //    the filesystem. "test_mount_fscontext_t" cannot associate with "tmpfs_t".
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_rootcontext_allowed_t:s0", [&] {
    EXPECT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0,
                      "rootcontext=test_u:object_r:test_mount_fscontext_t:s0"),
                SyscallFailsWithErrno(EACCES));
  }));

  // 3. Verify that mounting succeeds when both 'relabelfrom' and 'associate' are allowed, that the
  //    root node takes the requested label, and that the filesystem keeps its policy-defined one.
  //    The domain holds no "dir" class relabel permissions, confirming that they are not consulted.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_rootcontext_allowed_t:s0", [&] {
    ASSERT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, rootcontext),
                SyscallSucceeds());

    auto root_label = GetLabel(mount_dir.path());
    ASSERT_TRUE(root_label.is_ok()) << "failed to read root label: " << root_label.error_value();
    EXPECT_EQ(root_label.value(), "test_u:object_r:test_mount_rootcontext_t:s0");

    EXPECT_THAT(umount(mount_dir.path().c_str()), SyscallSucceeds());
  }));
}

// Verifies the checks applied when "fscontext=" and "context=" are combined, giving the filesystem
// and the nodes it contains different labels.
TEST(MountTest, ContextWithFsContextRequiresRelabelFromAndAssociate) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping suite.";
  }

  const char* fscontext = "fscontext=test_u:object_r:test_mount_fscontext_t:s0";
  std::string options = std::string(fscontext) + ",context=test_u:object_r:test_mount_context_t:s0";
  test_helper::ScopedTempDir mount_dir;

  auto enforce = ScopedEnforcement::SetEnforcing();

  // 1. Overriding the node labels requires 'relabelfrom' to the filesystem's new label, which
  //    "test_mount_relabel_allowed_t" does not hold.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_relabel_allowed_t:s0", [&] {
    EXPECT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, options.c_str()),
                SyscallFailsWithErrno(EACCES));
  }));

  // 2. Verify that the 'associate' check is made against the new label: "test_mount_rootcontext_t"
  //    may associate with "tmpfs_t", but not with "test_mount_fscontext_t".
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_context_and_fscontext_t:s0", [&] {
    std::string bad_options =
        std::string(fscontext) + ",context=test_u:object_r:test_mount_rootcontext_t:s0";
    EXPECT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, bad_options.c_str()),
                SyscallFailsWithErrno(EACCES));
  }));

  // 3. Verify that the nodes take the "context=" label, rather than the "fscontext=" one.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_context_and_fscontext_t:s0", [&] {
    ASSERT_THAT(mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, options.c_str()),
                SyscallSucceeds());

    auto root_label = GetLabel(mount_dir.path());
    ASSERT_TRUE(root_label.is_ok()) << "failed to read root label: " << root_label.error_value();
    EXPECT_EQ(root_label.value(), "test_u:object_r:test_mount_context_t:s0");

    EXPECT_THAT(umount(mount_dir.path().c_str()), SyscallSucceeds());
  }));
}

TEST(MountTest, BindRemountWithContext) {
  test_helper::ScopedTempDir source_dir;
  test_helper::ScopedTempDir target_dir;
  const char* ctx1 = "context=test_u:object_r:test_mount_fscontext_t:s0";
  const char* ctx2 = "context=test_u:object_r:test_mount_relabel_allowed_t:s0";

  auto enforce = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_mount_permissive_t:s0", [&] {
    // 1. Initial mount of tmpfs with ctx1
    ASSERT_THAT(mount("tmpfs", source_dir.path().c_str(), "tmpfs", 0, ctx1), SyscallSucceeds());

    // 2. Bind mount source to target
    ASSERT_THAT(mount(source_dir.path().c_str(), target_dir.path().c_str(), nullptr, MS_BIND, 0),
                SyscallSucceeds());

    // 3. Remount the bind mount with SAME context - should succeed
    EXPECT_THAT(mount(nullptr, target_dir.path().c_str(), nullptr, MS_BIND | MS_REMOUNT, ctx1),
                SyscallSucceeds());

    // 4. Remount the bind mount with DIFFERENT context - Linux allows this (options are
    // ignored/no-op)
    EXPECT_THAT(mount(nullptr, target_dir.path().c_str(), nullptr, MS_BIND | MS_REMOUNT, ctx2),
                SyscallSucceeds());

    // 5. Verify the context is still ctx1 (it was not changed by the remount)
    EXPECT_TRUE(cpp23::contains(MountOptionsFor(target_dir.path()), "test_mount_fscontext_t"));
  }));
}

// See https://fxbug.dev/398696694: Starnix doesn't support POSIX message queues (`mq_*` syscalls
// and the `mqueue` filesystem). Must pass if/when implemented.
TEST(MountTest, MqueueNotSupported) {
  EXPECT_THAT(
      syscall(SYS_mq_open, "test_mq", O_CREAT | O_RDWR, /* mode = */ 0600, /* attr = */ nullptr),
      SyscallFailsWithErrno(ENOSYS));

  test_helper::ScopedTempDir mount_dir;
  EXPECT_THAT(mount("mqueue", mount_dir.path().c_str(), "mqueue", /* mountflags = */ 0,
                    /* data = */ nullptr),
              SyscallFailsWithErrno(ENODEV));
}

// See https://fxbug.dev/398707799: Starnix doesn't support NFS filesystems (`nfs`, `nfs4`). Must
// pass if/when implemented.
TEST(MountTest, NfsNotSupported) {
  auto filesystems = ReadFile("/proc/filesystems");
  ASSERT_THAT(filesystems, SyscallResultIsOk());
  EXPECT_FALSE(cpp23::contains(filesystems.value(), "\tnfs\n"));
  EXPECT_FALSE(cpp23::contains(filesystems.value(), "\tnfs4\n"));

  test_helper::ScopedTempDir mount_dir;
  EXPECT_THAT(
      mount("none", mount_dir.path().c_str(), "nfs", /* mountflags = */ 0, /* data = */ nullptr),
      SyscallFailsWithErrno(ENODEV));
}

}  // namespace

extern std::string DoPrePolicyLoadWork() {
  for (const char* option : kSelinuxMountOptions) {
    test_helper::ScopedTempDir mount_dir;

    // Before policy load, any SELinux mount option should result in EINVAL because the kernel
    // cannot validate the context (or even know which options are valid).
    std::string options = std::string(option) + "=system_u:object_r:tmp_t:s0";
    if (mount("tmpfs", mount_dir.path().c_str(), "tmpfs", 0, options.c_str()) != -1) {
      g_pre_policy_mount_results.emplace(option, fit::ok());
    } else {
      g_pre_policy_mount_results.emplace(option, fit::error(errno));
    }
  }

  return "mount_policy";
}
