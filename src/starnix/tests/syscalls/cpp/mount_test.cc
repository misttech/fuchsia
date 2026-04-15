// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <dirent.h>
#include <fcntl.h>
#include <ftw.h>
#include <lib/stdcompat/string_view.h>
#include <mntent.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/statfs.h>
#include <sys/statvfs.h>
#include <unistd.h>

#include <cerrno>
#include <fstream>
#include <iostream>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/loop.h>

#include "src/lib/files/file.h"
#include "src/lib/fxl/strings/split_string.h"
#include "src/starnix/tests/syscalls/cpp/proc_test_base.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

using ::testing::IsSupersetOf;
using ::testing::UnorderedElementsAreArray;

static bool skip_mount_tests = false;

class MountTest : public ::testing::Test {
 public:
  static void SetUpTestSuite() {
    // The unshare() call will isolate the mount namespaces for the running
    // test process. This allows the Linux-based tests to execute syscalls with
    // root permissions, without fear of messing the environment up. While the
    // Starnix tests don't strictly need to unshare, it's beneficial to run the
    // same test binaries on Linux and on Starnix so we can be sure the semantics
    // match. As a side effect, this means that the mounted directories will not
    // be viewable in traditional ways, e.g. ffx component explore.
    // TODO(https://fxbug.dev/317285180) don't skip on baseline
    int rv = unshare(CLONE_NEWNS);
    if (rv == -1 && errno == EPERM) {
      // GTest does not support GTEST_SKIP() from a suite setup, so record that we want to skip
      // every test here and skip in SetUp().
      skip_mount_tests = true;
      return;
    }
    ASSERT_EQ(rv, 0) << "unshare(CLONE_NEWNS) failed: " << strerror(errno) << "(" << errno << ")";
  }

  void SetUp() override {
    if (skip_mount_tests) {
      GTEST_SKIP() << "Permission denied for unshare(CLONE_NEWNS), skipping suite.";
    }

    ASSERT_FALSE(temp_dir_.path().empty());
    ASSERT_THAT(mount(nullptr, temp_dir_.path().c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());

    MakeOwnMount("1");
    MakeDir("1/1");
    MakeOwnMount("2");
    MakeDir("2/2");

    ASSERT_TRUE(FileExists("1/1"));
    ASSERT_TRUE(FileExists("2/2"));
  }

  /// All paths used in test functions are relative to the temp directory. This function makes the
  /// path absolute.
  std::string TestPath(const char *path) const { return temp_dir_.path() + "/" + path; }

  // Create a directory.
  int MakeDir(const char *name) const {
    auto path = TestPath(name);
    return mkdir(path.c_str(), 0777);
  }

  // Create a file.
  fbl::unique_fd MakeFile(const char *name) const {
    auto path = TestPath(name);
    return fbl::unique_fd(open(path.c_str(), O_CREAT | O_RDWR, S_IRUSR | S_IWUSR));
  }

  /// Make the directory into a bind mount of itself.
  int MakeOwnMount(const char *name) const {
    int err = MakeDir(name);
    if (err < 0)
      return err;
    return Mount(name, name, MS_BIND);
  }

  // Call mount with a null fstype and data.
  int Mount(const char *src, const char *target, int flags) const {
    return mount(src == nullptr ? nullptr : TestPath(src).c_str(), TestPath(target).c_str(),
                 nullptr, flags, nullptr);
  }

  int Unmount(const char *path, int flags) const {
    return umount2(path == nullptr ? nullptr : TestPath(path).c_str(), flags);
  }

  ::testing::AssertionResult FileExists(const char *name) const {
    auto path = TestPath(name);
    if (access(path.c_str(), F_OK) != 0)
      return ::testing::AssertionFailure() << path << ": " << strerror(errno);
    return ::testing::AssertionSuccess();
  }

 private:
  test_helper::ScopedTempDir temp_dir_;
};

[[maybe_unused]] void DumpMountinfo() {
  int fd = open("/proc/self/mountinfo", O_RDONLY);
  char buf[10000];
  size_t n;
  while ((n = read(fd, buf, sizeof(buf))) > 0)
    write(STDOUT_FILENO, buf, n);
  close(fd);
}

#define ASSERT_SUCCESS(call) ASSERT_THAT((call), SyscallSucceeds())

TEST_F(MountTest, RecursiveBind) {
  // Make some mounts
  ASSERT_SUCCESS(MakeDir("a"));
  ASSERT_SUCCESS(Mount("1", "a", MS_BIND));
  ASSERT_SUCCESS(Mount("2", "a/1", MS_BIND));
  ASSERT_TRUE(FileExists("a/1"));
  ASSERT_TRUE(FileExists("a/1/2"));

  // Copy the tree
  ASSERT_SUCCESS(MakeDir("b"));
  ASSERT_SUCCESS(Mount("a", "b", MS_BIND | MS_REC));
  ASSERT_TRUE(FileExists("b/1"));
  ASSERT_TRUE(FileExists("b/1/2"));
}

TEST_F(MountTest, BindIgnoresSharingFlags) {
  ASSERT_SUCCESS(MakeDir("a"));
  // The bind mount should ignore the MS_SHARED flag, so we should end up with non-shared mounts.
  ASSERT_SUCCESS(Mount("1", "a", MS_BIND | MS_SHARED));
  ASSERT_SUCCESS(MakeDir("b"));
  ASSERT_SUCCESS(Mount("a", "b", MS_BIND | MS_SHARED));

  ASSERT_SUCCESS(Mount("2", "a/1", MS_BIND));
  ASSERT_TRUE(FileExists("a/1/2"));
  ASSERT_FALSE(FileExists("b/1/2"));
}

TEST_F(MountTest, BasicSharing) {
  ASSERT_SUCCESS(MakeDir("a"));
  ASSERT_SUCCESS(Mount("1", "a", MS_BIND));
  // Must be done in two steps! MS_BIND | MS_SHARED just ignores the MS_SHARED
  ASSERT_SUCCESS(Mount(nullptr, "a", MS_SHARED));
  ASSERT_SUCCESS(MakeDir("b"));
  ASSERT_SUCCESS(Mount("a", "b", MS_BIND));

  ASSERT_SUCCESS(Mount("2", "a/1", MS_BIND));
  ASSERT_TRUE(FileExists("a/1/2"));
  ASSERT_TRUE(FileExists("b/1/2"));
  ASSERT_FALSE(FileExists("1/1/2"));
}

TEST_F(MountTest, FlagVerification) {
  ASSERT_THAT(Mount(nullptr, "1", MS_SHARED | MS_PRIVATE), SyscallFailsWithErrno(EINVAL));
  ASSERT_THAT(Mount(nullptr, "1", MS_SHARED | MS_NOUSER), SyscallFailsWithErrno(EINVAL));
  ASSERT_THAT(Mount(nullptr, "1", MS_SHARED | MS_SILENT), SyscallSucceeds());
}

// Quiz question B from https://www.kernel.org/doc/Documentation/filesystems/sharedsubtree.txt
TEST_F(MountTest, QuizBRecursion) {
  // Create a hierarchy
  ASSERT_SUCCESS(MakeDir("a"));
  ASSERT_SUCCESS(Mount("1", "a", MS_BIND));
  ASSERT_SUCCESS(Mount("2", "a/1", MS_BIND));

  // Make it shared
  ASSERT_SUCCESS(Mount(nullptr, "a", MS_SHARED | MS_REC));

  // Clone it into itself
  ASSERT_SUCCESS(Mount("a", "a/1/2", MS_BIND | MS_REC));
  ASSERT_TRUE(FileExists("a/1/2/1/2"));
  ASSERT_FALSE(FileExists("a/1/2/1/2/1/2"));
  ASSERT_FALSE(FileExists("a/1/2/1/2/1/2/1/2"));
}

// Quiz question C from https://www.kernel.org/doc/Documentation/filesystems/sharedsubtree.txt
TEST_F(MountTest, QuizCPropagation) {
  ASSERT_SUCCESS(Mount(nullptr, "1", MS_SHARED));
  ASSERT_SUCCESS(MakeDir("1/1/2"));
  ASSERT_SUCCESS(MakeDir("1/1/2/3"));
  ASSERT_SUCCESS(MakeDir("1/1/test"));

  ASSERT_SUCCESS(MakeDir("a"));
  ASSERT_SUCCESS(Mount("1/1", "a", MS_BIND));
  ASSERT_SUCCESS(Mount(nullptr, "1", MS_SLAVE));
  ASSERT_SUCCESS(Mount(nullptr, "1", MS_SHARED));
  ASSERT_SUCCESS(MakeDir("b"));
  ASSERT_SUCCESS(Mount("1/1/2", "b", MS_BIND));
  ASSERT_SUCCESS(Mount(nullptr, "1", MS_SLAVE));

  ASSERT_SUCCESS(Mount("2", "a/test", MS_BIND));
  ASSERT_TRUE(FileExists("1/1/test/2"));
}

TEST_F(MountTest, PropagateOntoMountRoot) {
  ASSERT_SUCCESS(Mount(nullptr, "1", MS_SHARED));
  ASSERT_SUCCESS(MakeDir("1/1/1"));
  ASSERT_SUCCESS(MakeDir("a"));
  ASSERT_SUCCESS(Mount("1/1", "a", MS_BIND));
  // The propagation of this should be equivalent to shadowing the "a" mount.
  ASSERT_SUCCESS(Mount("2", "1/1", MS_BIND));
  ASSERT_TRUE(FileExists("a/2"));
}

TEST_F(MountTest, InheritShared) {
  ASSERT_SUCCESS(MakeDir("a"));
  ASSERT_SUCCESS(Mount("1", "a", MS_BIND));
  ASSERT_SUCCESS(Mount(nullptr, "a", MS_SHARED));
  ASSERT_SUCCESS(Mount("2", "a/1", MS_BIND));
  ASSERT_SUCCESS(MakeDir("b"));
  ASSERT_SUCCESS(Mount("a/1", "b", MS_BIND));
  ASSERT_SUCCESS(Mount("1", "b/2", MS_BIND));
  ASSERT_TRUE(FileExists("a/1/2/1"));
}

TEST_F(MountTest, LotsOfShadowing) {
  ASSERT_SUCCESS(MakeDir("a"));
  ASSERT_SUCCESS(Mount(nullptr, "1", MS_SHARED));
  ASSERT_SUCCESS(Mount("1", "a", MS_BIND));
  ASSERT_SUCCESS(Mount("1", "a", MS_BIND));
  ASSERT_SUCCESS(Mount("1", "a", MS_BIND));
}

TEST_F(MountTest, PropagateUnmount) {
  ASSERT_SUCCESS(MakeDir("a"));
  ASSERT_SUCCESS(Mount(nullptr, "1", MS_SHARED));
  ASSERT_SUCCESS(Mount("1", "a", MS_BIND));
  ASSERT_SUCCESS(Mount("2", "1/1", MS_BIND));
  ASSERT_TRUE(FileExists("1/1/2"));
  ASSERT_TRUE(FileExists("a/1/2"));

  ASSERT_SUCCESS(Unmount("1/1", 0));
  ASSERT_FALSE(FileExists("1/1/2"));
  ASSERT_FALSE(FileExists("a/1/2"));
}

// TODO(tbodt): write more tests:
// - A and B are shared, make B downstream, make A private, should now both be private

// Check that correct mount root is reported in in `/proc/<pid>/mountinfo`.
TEST_F(MountTest, ProcMountInfoRoot) {
  ASSERT_SUCCESS(MakeDir("a"));
  ASSERT_SUCCESS(MakeDir("a/foo"));
  ASSERT_SUCCESS(MakeDir("b"));
  ASSERT_SUCCESS(Mount("a/foo", "b", MS_BIND));

  auto info = test_helper::ReadMountInfoLine(TestPath("b"));
  ASSERT_TRUE(info.has_value());
  EXPECT_EQ(info->root, "/a/foo");

  ASSERT_THAT(rmdir(TestPath("a/foo").c_str()), SyscallSucceeds());

  info = test_helper::ReadMountInfoLine(TestPath("b"));
  ASSERT_TRUE(info.has_value());
  EXPECT_EQ(info->root, "/a/foo//deleted");
}

TEST_F(MountTest, Ext4ReadOnlySmokeTest) {
  std::string expected_contents;
  EXPECT_TRUE(files::ReadFileToString("data/tests/deps/hello_world.txt", &expected_contents));

  fbl::unique_fd loop_control(open("/dev/loop-control", O_RDWR, 0777));
  ASSERT_TRUE(loop_control.is_valid());

  int free_loop_device_num(ioctl(loop_control.get(), LOOP_CTL_GET_FREE, nullptr));
  ASSERT_TRUE(free_loop_device_num >= 0);

  std::string loop_device_path = "/dev/loop" + std::to_string(free_loop_device_num);
  fbl::unique_fd free_loop_device(open(loop_device_path.c_str(), O_RDONLY, 0644));
  ASSERT_TRUE(free_loop_device.is_valid());

  fbl::unique_fd ext_image(open("data/tests/deps/simple_ext4.img", O_RDONLY, 0644));
  ASSERT_TRUE(ext_image.is_valid());

  ASSERT_SUCCESS(ioctl(free_loop_device.get(), LOOP_SET_FD, ext_image.get()));

  ASSERT_SUCCESS(MakeDir("basic_ext4"));
  ASSERT_SUCCESS(
      mount(loop_device_path.c_str(), TestPath("basic_ext4").c_str(), "ext4", MS_RDONLY, nullptr));

  std::string observed_contents;
  EXPECT_TRUE(files::ReadFileToString(TestPath("basic_ext4/hello_world.txt"), &observed_contents));

  ASSERT_EQ(expected_contents, observed_contents);
}

TEST_F(MountTest, RemountReadOnlyToReadWriteIgnored) {
  // Create a tmpfs mount with a readonly superblock, and verify the reported mount flags.
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");
  ASSERT_THAT(mount(nullptr, dir.c_str(), "tmpfs", MS_RDONLY, nullptr), SyscallSucceeds());
  struct statfs64 fs_stat{};
  ASSERT_THAT(statfs64(dir.c_str(), &fs_stat), SyscallSucceeds());
  ASSERT_TRUE(fs_stat.f_flags & ST_RDONLY);

  // Attempt to bind remount as read-write, which will succeed without actually removing the
  // read-only mount flag.
  ASSERT_THAT(mount(nullptr, dir.c_str(), nullptr, MS_BIND | MS_REMOUNT, nullptr),
              SyscallSucceeds());
  ASSERT_THAT(statfs64(dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);
}

TEST_F(MountTest, RemountReadWriteToReadOnly) {
  // Create a tmpfs mount with a read-write superblock, and verify the reported mount flags.
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");
  ASSERT_THAT(mount(nullptr, dir.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  struct statfs64 fs_stat{};
  ASSERT_THAT(statfs64(dir.c_str(), &fs_stat), SyscallSucceeds());
  ASSERT_FALSE(fs_stat.f_flags & ST_RDONLY);

  // Remount read-only and verify the reported mount flags.
  ASSERT_THAT(mount(nullptr, dir.c_str(), nullptr, MS_BIND | MS_REMOUNT | MS_RDONLY, nullptr),
              SyscallSucceeds());
  ASSERT_THAT(statfs64(dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);

  // Remount back to read-write and verify the reported flags.
  ASSERT_THAT(mount(nullptr, dir.c_str(), nullptr, MS_BIND | MS_REMOUNT, nullptr),
              SyscallSucceeds());
  ASSERT_THAT(statfs64(dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_FALSE(fs_stat.f_flags & ST_RDONLY);
}

TEST_F(MountTest, RemountBindReadOnlyFlagInheritance) {
  // To validate propagation and inheritance of the `MS_RDONLY` flag between base and bind mounts
  // we create:
  //   base - A tmpfs instance created with an initially read-only superblock.
  //   bind1 & bind2 - Bind mounts created from "base" directly.
  //   sub_bind1 - Bind mount created from "bind1".

  ASSERT_SUCCESS(MakeDir("base"));
  auto base_dir = TestPath("base");
  ASSERT_THAT(mount(nullptr, base_dir.c_str(), "tmpfs", MS_RDONLY, nullptr), SyscallSucceeds());

  ASSERT_SUCCESS(MakeDir("bind1"));
  auto bind1_dir = TestPath("bind1");
  ASSERT_THAT(mount(base_dir.c_str(), bind1_dir.c_str(), nullptr, MS_BIND, nullptr),
              SyscallSucceeds());

  ASSERT_SUCCESS(MakeDir("sub_bind1"));
  auto sub_bind1_dir = TestPath("sub_bind1");
  ASSERT_THAT(mount(bind1_dir.c_str(), sub_bind1_dir.c_str(), nullptr, MS_BIND, nullptr),
              SyscallSucceeds());

  ASSERT_SUCCESS(MakeDir("bind2"));
  auto bind2_dir = TestPath("bind2");
  ASSERT_THAT(mount(base_dir.c_str(), bind2_dir.c_str(), nullptr, MS_BIND, nullptr),
              SyscallSucceeds());

  // Verify the initial states of all the mounts.
  struct statfs64 fs_stat{};
  ASSERT_THAT(statfs64(base_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);
  ASSERT_THAT(statfs64(bind1_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);
  ASSERT_THAT(statfs64(sub_bind1_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);
  ASSERT_THAT(statfs64(bind2_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);

  // Remounting "bind2" read-write will succeed, but have no visible effect because the superblock
  // is still read-write.
  ASSERT_THAT(mount(nullptr, bind2_dir.c_str(), nullptr, MS_BIND | MS_REMOUNT, nullptr),
              SyscallSucceeds());
  ASSERT_THAT(statfs64(bind2_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);

  // Remount "base" read-write and verify that this only affects "bind2", because we just cleared
  // the `MS_RDONLY` bit associated with that bind mount, and now the superblock is also read-write.
  // "sub_bind1" and "bind2" remaining read-only, having inherited `MS_RDONLY` at creation.
  ASSERT_THAT(mount(nullptr, base_dir.c_str(), nullptr, MS_REMOUNT, nullptr), SyscallSucceeds());
  ASSERT_THAT(statfs64(base_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_FALSE(fs_stat.f_flags & ST_RDONLY);

  ASSERT_THAT(statfs64(base_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_FALSE(fs_stat.f_flags & ST_RDONLY);
  ASSERT_THAT(statfs64(bind1_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);
  ASSERT_THAT(statfs64(sub_bind1_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);
  ASSERT_THAT(statfs64(bind2_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_FALSE(fs_stat.f_flags & ST_RDONLY);

  // Remounting "sub_bind1" read-write will succeed, even though "bind1", from which it was created
  // is still read-only, because it only inherits the superblock state.
  ASSERT_THAT(mount(nullptr, sub_bind1_dir.c_str(), nullptr, MS_BIND | MS_REMOUNT, nullptr),
              SyscallSucceeds());
  ASSERT_THAT(statfs64(sub_bind1_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_FALSE(fs_stat.f_flags & ST_RDONLY);

  // Remount the base filesystem read-only, and verify that all mounts are now read-only
  ASSERT_THAT(mount(nullptr, base_dir.c_str(), nullptr, MS_REMOUNT | MS_RDONLY, nullptr),
              SyscallSucceeds());
  ASSERT_THAT(statfs64(base_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);
  ASSERT_THAT(statfs64(bind1_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);
  ASSERT_THAT(statfs64(sub_bind1_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);
  ASSERT_THAT(statfs64(bind2_dir.c_str(), &fs_stat), SyscallSucceeds());
  EXPECT_TRUE(fs_stat.f_flags & ST_RDONLY);
}

// Test that we can successfully mount ext4 images backed files in an fs that returns resizable
// VMOs.
TEST_F(MountTest, Ext4ReadOnlyInMutableStorageSmokeTest) {
  std::string expected_contents;
  ASSERT_TRUE(files::ReadFileToString("data/tests/deps/hello_world.txt", &expected_contents));

  fbl::unique_fd loop_control(open("/dev/loop-control", O_RDWR, 0777));
  ASSERT_TRUE(loop_control.is_valid());

  int free_loop_device_num(ioctl(loop_control.get(), LOOP_CTL_GET_FREE, nullptr));
  ASSERT_TRUE(free_loop_device_num >= 0);

  std::string loop_device_path = "/dev/loop" + std::to_string(free_loop_device_num);
  fbl::unique_fd free_loop_device(open(loop_device_path.c_str(), O_RDONLY, 0644));
  ASSERT_TRUE(free_loop_device.is_valid());

  // Copy the original ext4 image to a location in mutable storage.
  std::ifstream orig_image("data/tests/deps/simple_ext4.img", std::ios_base::in | std::ios::binary);
  std::string image_in_mut_storage_path =
      std::string(std::getenv("MUTABLE_STORAGE")) + "/simple_ext4_in_mut_storage.img";
  std::ofstream image_in_mut_storage(image_in_mut_storage_path,
                                     std::ios_base::out | std::ios::binary);
  ASSERT_TRUE(orig_image.good());
  ASSERT_TRUE(image_in_mut_storage.good());
  char buf[4096];
  do {
    orig_image.read(&buf[0], 4096);
    image_in_mut_storage.write(&buf[0], orig_image.gcount());
  } while (orig_image.gcount() > 0);
  orig_image.close();
  image_in_mut_storage.close();

  fbl::unique_fd ext_image(open(image_in_mut_storage_path.c_str(), O_RDONLY, 0644));
  ASSERT_TRUE(ext_image.is_valid());

  ASSERT_SUCCESS(ioctl(free_loop_device.get(), LOOP_SET_FD, ext_image.get()));

  ASSERT_SUCCESS(MakeDir("basic_ext4"));
  ASSERT_SUCCESS(
      mount(loop_device_path.c_str(), TestPath("basic_ext4").c_str(), "ext4", MS_RDONLY, nullptr));

  std::string observed_contents;
  ASSERT_TRUE(files::ReadFileToString(TestPath("basic_ext4/hello_world.txt"), &observed_contents));

  ASSERT_EQ(expected_contents, observed_contents);
}

TEST_F(MountTest, BusyWithOpenFile) {
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");
  ASSERT_THAT(mount(nullptr, dir.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  fbl::unique_fd foo = MakeFile("a/foo");
  ASSERT_TRUE(foo.is_valid());
  ASSERT_THAT(umount(dir.c_str()), SyscallFailsWithErrno(EBUSY));
  foo.reset();
  ASSERT_THAT(umount(dir.c_str()), SyscallSucceeds());
}

TEST_F(MountTest, BusyWithCwd) {
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");
  ASSERT_THAT(mount(nullptr, dir.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  char original_cwd[PATH_MAX] = {};
  getcwd(original_cwd, sizeof(original_cwd));
  ASSERT_THAT(chdir(dir.c_str()), SyscallSucceeds());
  ASSERT_THAT(umount(dir.c_str()), SyscallFailsWithErrno(EBUSY));
  ASSERT_THAT(chdir(original_cwd), SyscallSucceeds());
  ASSERT_THAT(umount(dir.c_str()), SyscallSucceeds());
}

TEST_F(MountTest, BusyWithMmap) {
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");
  ASSERT_THAT(mount(nullptr, dir.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  fbl::unique_fd foo = MakeFile("a/foo");
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  void *mmap_addr = mmap(nullptr, page_size, PROT_READ, MAP_SHARED, foo.get(), 0);
  foo.reset();
  ASSERT_THAT(umount(dir.c_str()), SyscallFailsWithErrno(EBUSY));
  SAFE_SYSCALL(munmap(mmap_addr, page_size));
  ASSERT_THAT(umount(dir.c_str()), SyscallSucceeds());
}

TEST_F(MountTest, NoDev) {
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");
  ASSERT_THAT(mount(nullptr, dir.c_str(), "tmpfs", MS_NODEV, nullptr), SyscallSucceeds());
  auto path = TestPath("a/foo");
  ASSERT_THAT(mknod(path.c_str(), S_IFBLK | 0777, 0), SyscallSucceeds());
  ASSERT_THAT(open(path.c_str(), O_RDONLY), SyscallFailsWithErrno(EACCES));
  ASSERT_THAT(umount(dir.c_str()), SyscallSucceeds());
}

TEST_F(MountTest, CannotExecFromNoexecMount) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");
  ASSERT_THAT(mount(nullptr, dir.c_str(), "tmpfs", MS_NOEXEC, nullptr), SyscallSucceeds());

  auto path = dir + "/exe";
  // Copy /proc/self/exe to the new mount.
  std::string self_exe_contents;
  ASSERT_TRUE(files::ReadFileToString("/proc/self/exe", &self_exe_contents));
  ASSERT_TRUE(files::WriteFile(path, self_exe_contents));
  ASSERT_THAT(chmod(path.c_str(), 0755), SyscallSucceeds());

  // We should still be able to open the file for reading.
  ASSERT_THAT(fbl::unique_fd(open(path.c_str(), O_RDONLY)).get(), SyscallSucceeds());

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&path] {
    char *const argv[] = {const_cast<char *>(path.c_str()), nullptr};
    EXPECT_THAT(execve(path.c_str(), argv, nullptr), SyscallFailsWithErrno(EACCES));
  });
  ASSERT_TRUE(helper.WaitForChildren());

  ASSERT_THAT(umount(dir.c_str()), SyscallSucceeds());
}

TEST_F(MountTest, UmountIsNotRecursive) {
  // Test that by itself, umount should not umount nested mounts.
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");
  ASSERT_THAT(mount(nullptr, dir.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  ASSERT_SUCCESS(MakeDir("a/b"));
  auto nested_dir = TestPath("a/b");
  ASSERT_THAT(mount(nullptr, nested_dir.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  EXPECT_THAT(umount(dir.c_str()), SyscallFailsWithErrno(EBUSY));
  EXPECT_THAT(umount(nested_dir.c_str()), SyscallSucceeds());
  EXPECT_THAT(umount(dir.c_str()), SyscallSucceeds());
}

TEST_F(MountTest, UmountWithMntAttachIsRecursive) {
  // Test that when umount is called with `MNT_DETACH` it will umount nested
  // mounts.
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");
  ASSERT_THAT(mount(nullptr, dir.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  ASSERT_SUCCESS(MakeDir("a/b"));
  auto nested_dir = TestPath("a/b");
  ASSERT_THAT(mount(nullptr, nested_dir.c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());
  EXPECT_THAT(umount2(dir.c_str(), MNT_DETACH), SyscallSucceeds());
}

TEST_F(MountTest, BasicRemotefsMount) {
  // Basic remotefs mounting of the container's /data namespace
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "MountTest.BasicRemotefsMount cannot be run on Linux, skipping.";
  }

  // In order to validate that the /data directory is actually correctly
  // mounted and being serviced, we'll mount two target directories to it.
  // We can then create a file in one of the mounted directories, and confirm
  // that the file shows up in the second mounted directory. This is a
  // roundabout way to validate, since we do not have access to the container's
  // backing /data directory directly to validate file existence.
  ASSERT_SUCCESS(MakeDir("first_target"));
  auto first_target_dir = TestPath("first_target");
  ASSERT_THAT(mount("/data", first_target_dir.c_str(), "remotefs", MS_SYNCHRONOUS, nullptr),
              SyscallSucceeds());

  ASSERT_SUCCESS(MakeDir("second_target"));
  auto second_target_dir = TestPath("second_target");
  ASSERT_THAT(mount("/data", second_target_dir.c_str(), "remotefs", MS_SYNCHRONOUS, nullptr),
              SyscallSucceeds());

  // Create a file in the first mounted directory.
  auto new_file = first_target_dir + "/foo.txt";
  auto new_fd = fbl::unique_fd(open(new_file.c_str(), O_CREAT | O_RDWR, S_IRUSR | S_IWUSR, 0777));
  ASSERT_TRUE(new_fd.is_valid());
  new_fd.reset();

  // Confirm that we can find and open the file within the second directory.
  // In doing so, we can be reasonably assured that the common mount point (/data)
  // was correctly mounted and serviced by the system.
  auto previously_created_file = second_target_dir + "/foo.txt";
  auto previously_created_fd = fbl::unique_fd(open(previously_created_file.c_str(), O_RDONLY));
  ASSERT_TRUE(previously_created_fd.is_valid());
}

TEST_F(MountTest, RemotefsSubdirMount) {
  // Requested mounts of a nested path (e.g. /data/foo) with a valid namespace
  // in the ancestor path (e.g. /data) will be mounted. The kernel should
  // transparently create the subdir nodes in the remotefs mount call.
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "MountTest.RemotefsSubdirMount cannot be run on Linux, skipping.";
  }
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");
  ASSERT_THAT(mount("/data/foo", dir.c_str(), "remotefs", MS_RDONLY, nullptr), SyscallSucceeds());
}

TEST_F(MountTest, RemotefsInvalidPath) {
  // Remotefs should not mount if it's not provided a valid namespace path
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "MountTest.RemotefsInvalidPath cannot be run on Linux, skipping.";
  }
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");
  ASSERT_THAT(mount("/foo", dir.c_str(), "remotefs", MS_RDONLY, nullptr),
              SyscallFailsWithErrno(ENOENT));
}

TEST_F(MountTest, RemotefsRelativePath) {
  // Remotefs requires absolute paths, which correspond to a namespace entry.
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "MountTest.RemotefsNoPathProvided cannot be run on Linux, skipping.";
  }
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");
  ASSERT_THAT(mount(".", dir.c_str(), "remotefs", MS_RDONLY, nullptr), SyscallFailsWithErrno(2));
  ASSERT_THAT(mount("/", dir.c_str(), "remotefs", MS_RDONLY, nullptr), SyscallFailsWithErrno(2));
  ASSERT_THAT(mount("", dir.c_str(), "remotefs", MS_RDONLY, nullptr), SyscallFailsWithErrno(2));
}

class ProcMountsTest : public ProcTestBase {
  // Note that these tests can be affected by those in other suites e.g. a
  // MountTest above that doesn't clean up its mounts may change the value of
  // /proc/mounts observed by these tests. Ideally, we'd run a each suite in a
  // different process (and mount namespace, as is already the case) to minimise
  // the blast radius.

 public:
  std::vector<std::string> read_mounts() {
    std::vector<std::string> ret;

    std::ifstream ifs(proc_path() + "/mounts");
    std::string s;
    while (getline(ifs, s)) {
      ret.push_back(s);
    }
    return ret;
  }

  std::string MountOptionsFor(std::string_view mount_path) {
    FILE *mounts = setmntent("/proc/mounts", "r");
    std::string result;
    for (struct mntent *entry = 0; (entry = getmntent(mounts));) {
      if (mount_path == entry->mnt_dir) {
        result = entry->mnt_opts;
        break;
      }
    }
    endmntent(mounts);
    return result;
  }
};

TEST_F(ProcMountsTest, Basic) {
  // This test assumes the mounts are very specific, so is too brittle to run on Linux.
  // TODO(https://fxbug.dev/317285180) don't skip on baseline
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "ProcMountsTest::Basic can not be run on Linux, skipping.";
  }
  EXPECT_THAT(read_mounts(), IsSupersetOf({
                                 "data/system / remote_bundle ro,nosuid,nodev,relatime 0 0",
                                 "none /dev devtmpfs rw,nosuid,relatime 0 0",
                                 ". /tmp tmpfs rw,relatime 0 0",
                             }));
}

TEST_F(ProcMountsTest, MountAdded) {
  // TODO(https://fxbug.dev/317285180) don't skip on baseline
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }

  auto before_mounts = read_mounts();

  std::optional<test_helper::ScopedTempDir> temp_dir;
  temp_dir.emplace();

  ASSERT_THAT(chmod(temp_dir->path().c_str(), 0777), SyscallSucceeds());
  ASSERT_THAT(mount("testtmp", temp_dir->path().c_str(), "tmpfs", 0, nullptr), SyscallSucceeds());

  auto expected_mounts = before_mounts;
  std::string mount = "testtmp " + temp_dir->path() + " tmpfs rw,relatime 0 0";
  expected_mounts.push_back(mount);
  EXPECT_THAT(read_mounts(), UnorderedElementsAreArray(expected_mounts));

  // Clean-up.
  temp_dir.reset();

  EXPECT_THAT(read_mounts(), UnorderedElementsAreArray(before_mounts));
}

TEST_F(ProcMountsTest, RemountBindReadonlyFlagInheritance) {
  // TODO(https://fxbug.dev/317285180) don't skip on baseline
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }

  test_helper::ScopedTempDir base;
  ASSERT_THAT(mount(nullptr, base.path().c_str(), "tmpfs", MS_RDONLY, nullptr), SyscallSucceeds());

  test_helper::ScopedTempDir bind;
  ASSERT_THAT(mount(base.path().c_str(), bind.path().c_str(), nullptr, MS_BIND, nullptr),
              SyscallSucceeds());

  // Verify that both base and bind mounts are read-only initially.
  EXPECT_TRUE(cpp23::contains(MountOptionsFor(base.path()), "ro"));
  EXPECT_TRUE(cpp23::contains(MountOptionsFor(bind.path()), "ro"));

  // Remount "bind" read-write and verify that that has no effect on the flags reported.
  ASSERT_THAT(mount(nullptr, bind.path().c_str(), nullptr, MS_BIND | MS_REMOUNT, nullptr),
              SyscallSucceeds());

  // Verify that both base and bind mounts are still reported as read-only.
  EXPECT_TRUE(cpp23::contains(MountOptionsFor(base.path()), "ro"));
  EXPECT_TRUE(cpp23::contains(MountOptionsFor(bind.path()), "ro"));

  // Remount "base" read-write and verify that both mounts are now read-write.
  ASSERT_THAT(mount(nullptr, base.path().c_str(), nullptr, MS_REMOUNT, nullptr), SyscallSucceeds());
  EXPECT_TRUE(cpp23::contains(MountOptionsFor(base.path()), "rw"));
  EXPECT_TRUE(cpp23::contains(MountOptionsFor(bind.path()), "rw"));
}

// Validates that remote_bundle instances cannot be re-mounted read/write.
TEST_F(ProcMountsTest, RemoteBundleRemountReadOnlyToReadWrite) {
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "RemoteBundle only exists on Starnix, skipping.";
  }
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }

  // Identify a remote_bundle mount. In the Starnix test environment, "/" is typically a
  // remote_bundle mount.
  std::string remote_bundle_path;
  {
    auto mounts = test_helper::ReadMountInfo();
    ASSERT_TRUE(mounts.is_ok());
    for (const auto &info : mounts.value()) {
      remote_bundle_path = info.mount_point;
      break;
    }
  }

  if (remote_bundle_path.empty()) {
    GTEST_SKIP() << "No remote_bundle mount found, skipping.";
  }

  // Verify that the remote_bundle mount has the readonly flag set.
  ASSERT_TRUE(cpp23::contains(MountOptionsFor(remote_bundle_path), "ro"));

  // Create a bind mount of the remote bundle on a temporary directory, to avoid side-effects of
  // the root filesystem being overlaid with a LayeredFS during tests.
  test_helper::ScopedTempDir bind_dir;
  ASSERT_THAT(mount(remote_bundle_path.c_str(), bind_dir.path().c_str(), nullptr, MS_BIND, nullptr),
              SyscallSucceeds());

  // Attempt to remount as read-write (per-mount flag). This will succeed.
  ASSERT_THAT(mount(nullptr, bind_dir.path().c_str(), nullptr, MS_BIND | MS_REMOUNT, nullptr),
              SyscallSucceeds());

  // Because the superblock is read-only, the bind mount will remain reported readonly.
  EXPECT_TRUE(cpp23::contains(MountOptionsFor(bind_dir.path()), "ro"));
}

// Validates that a remote_bundle mount at the root of the container is read-only even when
// container features using `LayeredFS` are enabled, which is always the case in userspace tests.
TEST_F(ProcMountsTest, RemoteBundleAtRootIsReadOnly) {
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "RemoteBundle only exists on Starnix, skipping.";
  }
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }

  auto mount_info = test_helper::ReadMountInfoLine("/");
  if (mount_info->fs_type != "remote_bundle") {
    GTEST_SKIP() << "Not running with remote_bundle mounted at /, skipping.";
  }

  // Verify that the root (remote_bundle) filesystem is reported as readonly.
  EXPECT_TRUE(cpp23::contains(MountOptionsFor("/"), "ro"));

  // Attempt to remount as read-write (per-mount flag). This will succeed.
  ASSERT_THAT(mount(nullptr, "/", nullptr, MS_BIND | MS_REMOUNT | MS_NOSUID | MS_NODEV, nullptr),
              SyscallSucceeds());

  // Because the superblock is read-only, the bind mount will remain reported readonly.
  EXPECT_TRUE(cpp23::contains(MountOptionsFor("/"), "ro"));

  // Restore the mount to read-only to satisfy `ProcMountsTest.Basic` expectations. This will
  // succeed.
  ASSERT_THAT(mount(nullptr, "/", nullptr, MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV,
                    nullptr),
              SyscallSucceeds());
}

TEST_F(MountTest, RecFlagIsNotStored) {
  ASSERT_SUCCESS(MakeDir("a"));
  auto dir = TestPath("a");

  ASSERT_THAT(mount(nullptr, dir.c_str(), "tmpfs", MS_REC, nullptr), SyscallSucceeds());

  struct statfs64 fs_stat{};
  ASSERT_THAT(statfs64(dir.c_str(), &fs_stat), SyscallSucceeds());
  // There is no ST_* flag for this, since it should not be returned here, so use the MS_* version.
  EXPECT_FALSE(fs_stat.f_flags & MS_REC);

  ASSERT_THAT(umount(dir.c_str()), SyscallSucceeds());
}

struct ProcMountinfoTestParams {
  int flag;
  std::vector<std::string> expected_options;
};

const char *MountFlagName(int flag) {
  switch (flag) {
    case MS_RDONLY:
      return "MS_RDONLY";
    case MS_NOSUID:
      return "MS_NOSUID";
    case MS_NODEV:
      return "MS_NODEV";
    case MS_NOEXEC:
      return "MS_NOEXEC";
    case MS_NOATIME:
      return "MS_NOATIME";
    case MS_NODIRATIME:
      return "MS_NODIRATIME";
    case MS_LAZYTIME:
      return "MS_LAZYTIME";
    case MS_RELATIME:
      return "MS_RELATIME";
    case MS_STRICTATIME:
      return "MS_STRICTATIME";
    case MS_SYNCHRONOUS:
      return "MS_SYNCHRONOUS";
    case MS_DIRSYNC:
      return "MS_DIRSYNC";
    case MS_MANDLOCK:
      return "MS_MANDLOCK";
    case MS_SILENT:
      return "MS_SILENT";
    default:
      return "UNKNOWN";
  }
}

class ProcMountinfoPerMountFlagsTest
    : public ProcMountsTest,
      public ::testing::WithParamInterface<ProcMountinfoTestParams> {};

TEST_P(ProcMountinfoPerMountFlagsTest, FlagReporting) {
  // TODO(https://fxbug.dev/317285180) don't skip on baseline
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }

  test_helper::ScopedTempDir temp_dir;
  ASSERT_THAT(mount("none", temp_dir.path().c_str(), "tmpfs", GetParam().flag, nullptr),
              SyscallSucceeds());

  auto info = test_helper::ReadMountInfoLine(temp_dir.path());
  ASSERT_TRUE(info.has_value());

  auto mount_options =
      fxl::SplitString(info->mount_options, ",", fxl::kKeepWhitespace, fxl::kSplitWantAll);
  EXPECT_THAT(mount_options, UnorderedElementsAreArray(GetParam().expected_options));
}

INSTANTIATE_TEST_SUITE_P(
    MountTest, ProcMountinfoPerMountFlagsTest,
    ::testing::Values(ProcMountinfoTestParams{MS_RDONLY, {"ro", "relatime"}},
                      ProcMountinfoTestParams{MS_NOSUID, {"rw", "nosuid", "relatime"}},
                      ProcMountinfoTestParams{MS_NODEV, {"rw", "nodev", "relatime"}},
                      ProcMountinfoTestParams{MS_NOEXEC, {"rw", "noexec", "relatime"}},
                      ProcMountinfoTestParams{MS_NOATIME, {"rw", "noatime"}},
                      ProcMountinfoTestParams{MS_NODIRATIME, {"rw", "nodiratime", "relatime"}},
                      ProcMountinfoTestParams{MS_RELATIME, {"rw", "relatime"}},
                      ProcMountinfoTestParams{MS_STRICTATIME, {"rw"}}),
    [](const ::testing::TestParamInfo<ProcMountinfoTestParams> &info) {
      return MountFlagName(info.param.flag);
    });

class ProcMountinfoSuperblockFlagsTest
    : public ProcMountsTest,
      public ::testing::WithParamInterface<ProcMountinfoTestParams> {};

TEST_P(ProcMountinfoSuperblockFlagsTest, FlagReporting) {
  // TODO(https://fxbug.dev/317285180) don't skip on baseline
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Not running with sysadmin capabilities, skipping.";
  }

  test_helper::ScopedTempDir temp_dir;
  ASSERT_THAT(mount("none", temp_dir.path().c_str(), "tmpfs", GetParam().flag, nullptr),
              SyscallSucceeds());

  auto info = test_helper::ReadMountInfoLine(temp_dir.path());
  ASSERT_TRUE(info.has_value());

  auto super_options_vec =
      fxl::SplitString(info->super_options, ",", fxl::kKeepWhitespace, fxl::kSplitWantAll);
  std::set<std::string_view> super_options(super_options_vec.begin(), super_options_vec.end());
  super_options.erase("inode64");

  EXPECT_THAT(super_options, UnorderedElementsAreArray(GetParam().expected_options));
}

INSTANTIATE_TEST_SUITE_P(MountTest, ProcMountinfoSuperblockFlagsTest,
                         ::testing::Values(ProcMountinfoTestParams{MS_RDONLY, {"ro"}},
                                           ProcMountinfoTestParams{MS_SYNCHRONOUS, {"rw", "sync"}},
                                           ProcMountinfoTestParams{MS_DIRSYNC, {"rw", "dirsync"}},
                                           ProcMountinfoTestParams{MS_MANDLOCK, {"rw", "mand"}},
                                           ProcMountinfoTestParams{MS_SILENT, {"rw"}},
                                           ProcMountinfoTestParams{MS_LAZYTIME,
                                                                   {"rw", "lazytime"}}),
                         [](const ::testing::TestParamInfo<ProcMountinfoTestParams> &info) {
                           return MountFlagName(info.param.flag);
                         });

TEST_F(MountTest, RelatimeIsDefault) {
  test_helper::ScopedTempDir atime_default;
  auto dir = atime_default.path();

  // Mount with no explicit access-time flags (flags = 0).
  auto mount = ASSERT_RESULT_SUCCESS_AND_RETURN(
      test_helper::ScopedMount::Mount("none", dir, "tmpfs", 0, nullptr));

  // Use the helper to read mount info for this path.
  auto info = test_helper::ReadMountInfoLine(dir);
  ASSERT_TRUE(info.has_value());

  // Check that "relatime" is in the mount options.
  EXPECT_TRUE(cpp23::contains(info->mount_options, "relatime")) << info->mount_options;

  // Also verify that conflicting flags like "noatime" or "strictatime" are NOT present.
  EXPECT_FALSE(cpp23::contains(info->mount_options, "noatime"));
  EXPECT_FALSE(cpp23::contains(info->mount_options, "strictatime"));
}

TEST_F(MountTest, BindMountInheritsAtimeFlags) {
  // Create a base mount with an explicit non-default exclusive flag MS_STRICTATIME, plus the
  // MS_NODIRATIME modifier flag.
  test_helper::ScopedTempDir base;
  auto base_dir = base.path();
  ASSERT_THAT(mount("tmpfs", base_dir.c_str(), "tmpfs", MS_STRICTATIME | MS_NODIRATIME, nullptr),
              SyscallSucceeds());

  // Create a bind mount from that base.
  test_helper::ScopedTempDir bind;
  auto bind_dir = bind.path();
  ASSERT_THAT(mount(base_dir.c_str(), bind_dir.c_str(), nullptr, MS_BIND, nullptr),
              SyscallSucceeds());

  // Verify that both the base and the bind mount report "nodiratime", and neither "relatime" nor
  // "noatime" (which implies strict atime).
  auto base_info = test_helper::ReadMountInfoLine(base_dir);
  ASSERT_TRUE(base_info.has_value());
  EXPECT_TRUE(cpp23::contains(base_info->mount_options, "nodiratime"));
  EXPECT_FALSE(cpp23::contains(base_info->mount_options, "noatime"));
  EXPECT_FALSE(cpp23::contains(base_info->mount_options, "relatime"));

  auto bind_info = test_helper::ReadMountInfoLine(bind_dir);
  ASSERT_TRUE(bind_info.has_value());
  EXPECT_TRUE(cpp23::contains(bind_info->mount_options, "nodiratime"));
  EXPECT_FALSE(cpp23::contains(bind_info->mount_options, "noatime"));
  EXPECT_FALSE(cpp23::contains(bind_info->mount_options, "relatime"));

  // Bind remounting with only MS_NODIRATIME should also implicitly switch to MS_RELATIME.
  ASSERT_THAT(
      mount(nullptr, bind_dir.c_str(), nullptr, MS_BIND | MS_REMOUNT | MS_NODIRATIME, nullptr),
      SyscallSucceeds());
  bind_info = test_helper::ReadMountInfoLine(bind_dir);
  ASSERT_TRUE(bind_info.has_value());
  EXPECT_TRUE(cpp23::contains(bind_info->mount_options, "nodiratime"));
  EXPECT_TRUE(cpp23::contains(bind_info->mount_options, "relatime"));
  EXPECT_FALSE(cpp23::contains(bind_info->mount_options, "noatime"));

  // Bind remounting with only MS_STRICTATIME should now clear both MS_RELATIME and MS_NODIRATIME.
  ASSERT_THAT(
      mount(nullptr, bind_dir.c_str(), nullptr, MS_BIND | MS_REMOUNT | MS_STRICTATIME, nullptr),
      SyscallSucceeds());
  bind_info = test_helper::ReadMountInfoLine(bind_dir);
  ASSERT_TRUE(bind_info.has_value());
  EXPECT_FALSE(cpp23::contains(bind_info->mount_options, "nodiratime"));
  EXPECT_FALSE(cpp23::contains(bind_info->mount_options, "relatime"));
  EXPECT_FALSE(cpp23::contains(bind_info->mount_options, "noatime"));
}

}  // namespace
