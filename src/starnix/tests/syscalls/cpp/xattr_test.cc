// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <string>
#include <vector>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <linux/capability.h>
#include <linux/limits.h>

#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

constexpr char kAttrName[] = "user.test";

class XattrTest : public ::testing::Test {
 protected:
  void SetUp() override {
    if (!test_helper::HasSysAdmin()) {
      GTEST_SKIP() << "Requires CAP_SYS_ADMIN to mount tmpfs.";
    }

    auto mount_result =
        test_helper::ScopedMount::Mount("none", temp_dir_.path(), "tmpfs", 0, nullptr);
    ASSERT_THAT(mount_result, SyscallResultIsOk()) << "Failed to mount tmpfs";
    mount_ = std::move(mount_result.value());

    test_file_path_ = temp_dir_.path() + "/xattr_test_file";
    fd_ = fbl::unique_fd(open(test_file_path_.c_str(), O_CREAT | O_RDWR, 0644));
    ASSERT_TRUE(fd_.is_valid()) << strerror(errno);
  }

  void TearDown() override {
    fd_.reset();
    if (!test_file_path_.empty()) {
      unlink(test_file_path_.c_str());
    }
    mount_ = test_helper::ScopedMount();
  }

  int fd() const { return fd_.get(); }
  const std::string& path() const { return test_file_path_; }

 private:
  test_helper::ScopedTempDir temp_dir_;
  test_helper::ScopedMount mount_;
  std::string test_file_path_;
  fbl::unique_fd fd_;
};

TEST_F(XattrTest, SetxattrValueLengthLimits) {
  // Value of size XATTR_SIZE_MAX (64 KiB = 65536 bytes):
  // Maximum allowed extended attribute value size.
  std::string val_64k(XATTR_SIZE_MAX, 'a');
  EXPECT_THAT(setxattr(path().c_str(), kAttrName, val_64k.data(), val_64k.size(), 0),
              SyscallSucceeds());

  std::vector<char> buf(XATTR_SIZE_MAX + 1);
  EXPECT_THAT(getxattr(path().c_str(), kAttrName, buf.data(), buf.size()),
              SyscallSucceedsWithValue(static_cast<ssize_t>(val_64k.size())));
  EXPECT_EQ(std::string(buf.data(), val_64k.size()), val_64k);

  // Value of size XATTR_SIZE_MAX + 1 (65537 bytes):
  // Exceeds XATTR_SIZE_MAX, so setxattr must fail with E2BIG.
  std::string val_too_big(XATTR_SIZE_MAX + 1, 'b');
  EXPECT_THAT(setxattr(path().c_str(), kAttrName, val_too_big.data(), val_too_big.size(), 0),
              SyscallFailsWithErrno(E2BIG));
}

TEST_F(XattrTest, FsetxattrValueLengthLimits) {
  // Value of size XATTR_SIZE_MAX (64 KiB) via fsetxattr.
  std::string val_64k(XATTR_SIZE_MAX, 'a');
  EXPECT_THAT(fsetxattr(fd(), kAttrName, val_64k.data(), val_64k.size(), 0), SyscallSucceeds());

  std::vector<char> buf(XATTR_SIZE_MAX + 1);
  EXPECT_THAT(fgetxattr(fd(), kAttrName, buf.data(), buf.size()),
              SyscallSucceedsWithValue(static_cast<ssize_t>(val_64k.size())));
  EXPECT_EQ(std::string(buf.data(), val_64k.size()), val_64k);

  // Value of size XATTR_SIZE_MAX + 1 via fsetxattr fails with E2BIG.
  std::string val_too_big(XATTR_SIZE_MAX + 1, 'b');
  EXPECT_THAT(fsetxattr(fd(), kAttrName, val_too_big.data(), val_too_big.size(), 0),
              SyscallFailsWithErrno(E2BIG));
}

TEST_F(XattrTest, LsetxattrValueLengthLimits) {
  // Value of size XATTR_SIZE_MAX (64 KiB) via lsetxattr.
  std::string val_64k(XATTR_SIZE_MAX, 'a');
  EXPECT_THAT(lsetxattr(path().c_str(), kAttrName, val_64k.data(), val_64k.size(), 0),
              SyscallSucceeds());

  std::vector<char> buf(XATTR_SIZE_MAX + 1);
  EXPECT_THAT(lgetxattr(path().c_str(), kAttrName, buf.data(), buf.size()),
              SyscallSucceedsWithValue(static_cast<ssize_t>(val_64k.size())));
  EXPECT_EQ(std::string(buf.data(), val_64k.size()), val_64k);

  // Value of size XATTR_SIZE_MAX + 1 via lsetxattr fails with E2BIG.
  std::string val_too_big(XATTR_SIZE_MAX + 1, 'b');
  EXPECT_THAT(lsetxattr(path().c_str(), kAttrName, val_too_big.data(), val_too_big.size(), 0),
              SyscallFailsWithErrno(E2BIG));
}

TEST_F(XattrTest, SetxattrNameLengthLimits) {
  // Name of length XATTR_NAME_MAX (255 bytes): "user." + 250 characters.
  std::string name_255 = "user." + std::string(XATTR_NAME_MAX - 5, 'c');
  ASSERT_EQ(name_255.size(), static_cast<size_t>(XATTR_NAME_MAX));
  EXPECT_THAT(setxattr(path().c_str(), name_255.c_str(), "val", 3, 0), SyscallSucceeds());

  // Name of length XATTR_NAME_MAX + 1 (256 bytes): "user." + 251 characters -> ERANGE.
  std::string name_256 = "user." + std::string(XATTR_NAME_MAX - 4, 'd');
  ASSERT_EQ(name_256.size(), static_cast<size_t>(XATTR_NAME_MAX + 1));
  EXPECT_THAT(setxattr(path().c_str(), name_256.c_str(), "val", 3, 0),
              SyscallFailsWithErrno(ERANGE));
}

TEST_F(XattrTest, SecuritySelinuxRequiresCapSysAdminWhenSelinuxDisabled) {
  test_helper::ScopedTempDir selinuxfs_dir;
  if (access("/sys/fs/selinux/enforce", F_OK) == 0 ||
      test_helper::ScopedMount::Mount("none", selinuxfs_dir.path(), "selinuxfs", 0, nullptr)
          .is_ok()) {
    GTEST_SKIP() << "Requires SELinux to be disabled in the kernel.";
  }

  constexpr char kSelinuxAttr[] = "security.selinux";
  constexpr char kVal[] = "u:object_r:unlabeled:s0";

  // Without CAP_SYS_ADMIN, setting security.selinux must fail with EPERM.
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([this, kSelinuxAttr, kVal] {
    test_helper::UnsetCapabilityEffective(CAP_SYS_ADMIN);
    EXPECT_THAT(setxattr(path().c_str(), kSelinuxAttr, kVal, sizeof(kVal), 0),
                SyscallFailsWithErrno(EPERM));
    EXPECT_THAT(fsetxattr(fd(), kSelinuxAttr, kVal, sizeof(kVal), 0), SyscallFailsWithErrno(EPERM));
    EXPECT_THAT(lsetxattr(path().c_str(), kSelinuxAttr, kVal, sizeof(kVal), 0),
                SyscallFailsWithErrno(EPERM));
  });
  EXPECT_TRUE(helper.WaitForChildren());

  // With CAP_SYS_ADMIN, setting security.selinux succeeds.
  ASSERT_THAT(setxattr(path().c_str(), kSelinuxAttr, kVal, sizeof(kVal), 0), SyscallSucceeds());

  // Without CAP_SYS_ADMIN, removing security.selinux must fail with EPERM.
  helper.RunInForkedProcess([this, kSelinuxAttr] {
    test_helper::UnsetCapabilityEffective(CAP_SYS_ADMIN);
    EXPECT_THAT(removexattr(path().c_str(), kSelinuxAttr), SyscallFailsWithErrno(EPERM));
  });
  EXPECT_TRUE(helper.WaitForChildren());

  // With CAP_SYS_ADMIN, removing security.selinux succeeds.
  EXPECT_THAT(removexattr(path().c_str(), kSelinuxAttr), SyscallSucceeds());
}

}  // namespace
