// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <dirent.h>
#include <fcntl.h>
#include <ftw.h>
#include <lib/fit/defer.h>
#include <lib/stdcompat/string_view.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <cerrno>
#include <fstream>
#include <iostream>

#include <gtest/gtest.h>
#include <linux/fs.h>
#include <linux/loop.h>

#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

#ifndef LOOP_CONFIGURE
struct loop_config {
  uint32_t fd;
  uint32_t block_size;
  struct loop_info64 info;
  uint64_t __reserved[8];
};

#define LOOP_CONFIGURE 0x4C0A
#endif  // LOOP_CONFIGURE

namespace {

bool skip_loop_tests = false;

class LoopTest : public ::testing::Test {
 public:
  static void SetUpTestSuite() {
    // TODO(https://fxbug.dev/317285180) don't skip on baseline
    int fd = open("/dev/loop-control", O_RDWR, 0777);
    if (fd == -1 && (errno == EACCES || errno == ENOENT)) {
      // GTest does not support GTEST_SKIP() from a suite setup, so record that we want to skip
      // every test here and skip in SetUp().
      skip_loop_tests = true;
      return;
    }
    ASSERT_TRUE(fd >= 0) << "open(\"/dev/loop-control\") failed: " << strerror(errno) << "("
                         << errno << ")";
  }

  void SetUp() override {
    if (skip_loop_tests) {
      GTEST_SKIP() << "Permission denied for /dev/loop-control, skipping suite.";
    }
    loop_control_ = fbl::unique_fd(open("/dev/loop-control", O_RDWR));
    ASSERT_TRUE(loop_control_.is_valid());
  }

  int GetFreeLoopDeviceNumber() {
    int free_loop_device_num(ioctl(loop_control_.get(), LOOP_CTL_GET_FREE, nullptr));
    EXPECT_TRUE(free_loop_device_num >= 0);
    return free_loop_device_num;
  }

  int RemoveLoopDevice(int loop_device_num) {
    int removed_loop_device_num(ioctl(loop_control_.get(), LOOP_CTL_REMOVE, loop_device_num));
    EXPECT_TRUE(removed_loop_device_num >= 0);
    return removed_loop_device_num;
  }

  int AddLoopDevice(int loop_device_num) {
    return ioctl(loop_control_.get(), LOOP_CTL_ADD, loop_device_num);
  }

  // Adds a loop device with the lowest unused minor number >= kFirstPrivateMinor and returns that
  // number. LOOP_CTL_GET_FREE hands out the lowest unbound device, so starting well above it keeps
  // other tests, which may run concurrently, from picking up this device. Records a test failure
  // and returns -1 if LOOP_CTL_ADD fails with anything but EEXIST.
  int AddUnusedLoopDevice() {
    constexpr int kFirstPrivateMinor = 64;
    for (int minor = kFirstPrivateMinor;; ++minor) {
      if (AddLoopDevice(minor) >= 0) {
        return minor;
      }
      if (errno != EEXIST) {
        ADD_FAILURE() << "LOOP_CTL_ADD(" << minor << ") failed: " << strerror(errno);
        return -1;
      }
    }
  }

  // Issues LOOP_CTL_REMOVE, retrying while it fails with EBUSY. On Linux, the backing file is
  // detached on last close after LOOP_CLR_FD, and udev may briefly hold a new device open, so
  // removal can transiently fail with EBUSY. Does not record test failures.
  int RemoveLoopDeviceRetryingOnBusy(int loop_device_num) {
    constexpr int kMaxAttempts = 50;
    int result = -1;
    for (int attempt = 0; attempt < kMaxAttempts; ++attempt) {
      result = ioctl(loop_control_.get(), LOOP_CTL_REMOVE, loop_device_num);
      if (result >= 0 || errno != EBUSY) {
        break;
      }
      usleep(100'000);
    }
    return result;
  }

  // RAII helper to manage the lifecycle of a bound loop device.
  class ActiveLoopDevice {
   public:
    ActiveLoopDevice(LoopTest& test, int backing_fd, uint32_t block_size = 4096,
                     uint64_t size_limit = 0, uint64_t offset = 0) {
      minor_ = ioctl(test.loop_control_.get(), LOOP_CTL_GET_FREE, nullptr);
      if (minor_ < 0)
        return;
      path_ = "/dev/loop" + std::to_string(minor_);
      fd_ = fbl::unique_fd(open(path_.c_str(), O_RDWR));
      if (!fd_.is_valid()) {
        minor_ = -1;
        return;
      }

      loop_config config = {.fd = static_cast<uint32_t>(backing_fd),
                            .block_size = block_size,
                            .info = {.lo_offset = offset, .lo_sizelimit = size_limit}};
      if (ioctl(fd_.get(), LOOP_CONFIGURE, &config) < 0) {
        fd_.reset();
        minor_ = -1;
      }
    }

    ~ActiveLoopDevice() {
      if (fd_.is_valid()) {
        ioctl(fd_.get(), LOOP_CLR_FD, 0);
      }
    }

    bool is_valid() const { return minor_ >= 0; }
    int minor() const { return minor_; }
    int fd() const { return fd_.get(); }
    const std::string& path() const { return path_; }

   private:
    int minor_ = -1;
    std::string path_;
    fbl::unique_fd fd_;
  };

 private:
  fbl::unique_fd loop_control_;
};

#define ASSERT_SUCCESS(call) ASSERT_THAT((call), SyscallSucceeds())

TEST_F(LoopTest, ReopeningDevicePreservesOffset) {
  fbl::unique_fd backing_file(open("data/hello_world.txt", O_RDONLY, 0644));
  ASSERT_TRUE(backing_file.is_valid());

  // Configure an offset that we'll check for after re-opening the device.
  ActiveLoopDevice device(*this, backing_file.get(), 4096, 0, 4096);
  ASSERT_TRUE(device.is_valid());

  loop_info64 first_observed_info;
  ASSERT_SUCCESS(ioctl(device.fd(), LOOP_GET_STATUS64, &first_observed_info));

  // Close the loop device fd and reopen it, confirming that the offset and other configuration are
  // the same and preserved even when there are no open files to the device.
  std::string device_path = device.path();
  fbl::unique_fd reopened_fd(open(device_path.c_str(), O_RDONLY, 0644));
  ASSERT_TRUE(reopened_fd.is_valid());
  loop_info64 second_observed_info;
  ASSERT_SUCCESS(ioctl(reopened_fd.get(), LOOP_GET_STATUS64, &second_observed_info));
  EXPECT_EQ(first_observed_info.lo_offset, second_observed_info.lo_offset);
}

TEST_F(LoopTest, RemoveLoopDeviceFromKernelDeviceRegistry) {
  int test_minor = AddUnusedLoopDevice();
  ASSERT_GE(test_minor, 0);
  int removed_loop_device_num = RemoveLoopDevice(test_minor);
  EXPECT_EQ(removed_loop_device_num, test_minor);
  std::string devfs_path = "/dev/";
  DIR* dir = opendir(devfs_path.c_str());
  ASSERT_TRUE(dir);
  while (struct dirent* entry = readdir(dir)) {
    std::string name = entry->d_name;
    if (cpp23::contains(name, "loop")) {
      std::string device_path = "/dev/" + name;
      int device_fd = open(device_path.c_str(), O_RDONLY);
      ASSERT_GT(device_fd, 0) << "device path is: " << device_path << strerror(errno);
    }
  }
  closedir(dir);
}

TEST_F(LoopTest, RemoveAfterClearFd) {
  fbl::unique_fd backing_file(open("data/hello_world.txt", O_RDONLY, 0644));
  ASSERT_TRUE(backing_file.is_valid());

  int test_minor = AddUnusedLoopDevice();
  ASSERT_GE(test_minor, 0);

  std::string device_path = "/dev/loop" + std::to_string(test_minor);
  fbl::unique_fd loop_fd;
  bool device_registered = true;
  // Detach the backing file, remove the loop device and unlink its node on every exit path,
  // including early returns from failed assertions. The unlink cleans up a leaked /dev/loop<N>
  // node (e.g. when removal fails) so that subsequent tests scanning /dev do not fail; it is
  // expected to fail with ENOENT after a successful removal.
  auto cleanup = fit::defer([&] {
    if (loop_fd.is_valid()) {
      ioctl(loop_fd.get(), LOOP_CLR_FD, 0);
      loop_fd.reset();
    }
    if (device_registered) {
      RemoveLoopDeviceRetryingOnBusy(test_minor);
    }
    unlink(device_path.c_str());
  });

  loop_fd.reset(open(device_path.c_str(), O_RDWR));
  ASSERT_TRUE(loop_fd.is_valid()) << strerror(errno);

  ASSERT_SUCCESS(ioctl(loop_fd.get(), LOOP_SET_FD, backing_file.get()));
  ASSERT_SUCCESS(ioctl(loop_fd.get(), LOOP_CLR_FD, 0));
  loop_fd.reset();

  ASSERT_SUCCESS(RemoveLoopDeviceRetryingOnBusy(test_minor));
  device_registered = false;
  ASSERT_SUCCESS(AddLoopDevice(test_minor));
  device_registered = true;
  ASSERT_SUCCESS(RemoveLoopDeviceRetryingOnBusy(test_minor));
  device_registered = false;
}

TEST_F(LoopTest, SysfsDeviceDirectoryRemovalAndReplacement) {
  int test_minor = AddUnusedLoopDevice();
  ASSERT_GE(test_minor, 0);

  std::string device_path = "/dev/loop" + std::to_string(test_minor);
  std::string sys_block_link = "/sys/block/loop" + std::to_string(test_minor);
  std::string sys_device_dir = "/sys/devices/virtual/block/loop" + std::to_string(test_minor);

  bool device_registered = true;
  auto cleanup = fit::defer([&] {
    if (device_registered && RemoveLoopDeviceRetryingOnBusy(test_minor) < 0) {
      unlink(device_path.c_str());
    }
  });

  // Look up the `/sys/block` symlink and target device directory before removal so both dentries
  // are populated in the VFS cache.
  struct stat first_lstat = {};
  ASSERT_SUCCESS(lstat(sys_block_link.c_str(), &first_lstat));
  struct stat first_stat = {};
  ASSERT_SUCCESS(stat(sys_block_link.c_str(), &first_stat));
  struct stat first_dir_stat = {};
  ASSERT_SUCCESS(stat(sys_device_dir.c_str(), &first_dir_stat));
  EXPECT_EQ(first_stat.st_ino, first_dir_stat.st_ino);

  // Removing and re-adding the loop device without an intervening sysfs lookup must replace the
  // cached sysfs symlink and target directory with new nodes rather than returning stale cached
  // dentries.
  ASSERT_SUCCESS(RemoveLoopDeviceRetryingOnBusy(test_minor));
  device_registered = false;
  ASSERT_SUCCESS(AddLoopDevice(test_minor));
  device_registered = true;

  struct stat second_lstat = {};
  ASSERT_SUCCESS(lstat(sys_block_link.c_str(), &second_lstat));
  EXPECT_NE(first_lstat.st_ino, second_lstat.st_ino);
  struct stat second_stat = {};
  ASSERT_SUCCESS(stat(sys_block_link.c_str(), &second_stat));
  EXPECT_NE(first_stat.st_ino, second_stat.st_ino);
  struct stat second_dir_stat = {};
  ASSERT_SUCCESS(stat(sys_device_dir.c_str(), &second_dir_stat));
  EXPECT_EQ(second_stat.st_ino, second_dir_stat.st_ino);

  // Removing the loop device must evict the cached sysfs entries so subsequent lookups fail with
  // ENOENT.
  ASSERT_SUCCESS(RemoveLoopDeviceRetryingOnBusy(test_minor));
  device_registered = false;
  struct stat removed_lstat = {};
  EXPECT_THAT(lstat(sys_block_link.c_str(), &removed_lstat), SyscallFailsWithErrno(ENOENT));
  EXPECT_THAT(access(sys_block_link.c_str(), F_OK), SyscallFailsWithErrno(ENOENT));
  EXPECT_THAT(access(sys_device_dir.c_str(), F_OK), SyscallFailsWithErrno(ENOENT));
}

TEST_F(LoopTest, BackingFile) {
  fbl::unique_fd backing_file(open("data/hello_world.txt", O_RDONLY, 0644));
  ASSERT_TRUE(backing_file.is_valid());

  ActiveLoopDevice device(*this, backing_file.get(), 4096, 0, 0);
  ASSERT_TRUE(device.is_valid());

  loop_info64 first_observed_info;
  ASSERT_SUCCESS(ioctl(device.fd(), LOOP_GET_STATUS64, &first_observed_info));

  std::string sys_backing_file_path =
      "/sys/block/loop" + std::to_string(device.minor()) + "/loop/backing_file";
  std::ifstream in(sys_backing_file_path);
  ASSERT_TRUE(in.is_open());
  std::stringstream buffer;
  buffer << in.rdbuf();
  // The actual path is prefixed with container namespace, which includes a dynamically
  // generated component identifier.  Since we don't want a change-detector test, just match
  // the suffix.
  ASSERT_TRUE(buffer.str().ends_with("/data/hello_world.txt\n"));
}

TEST_F(LoopTest, SysfsLoopDirectoryLifecycle) {
  fbl::unique_fd backing_file(open("data/hello_world.txt", O_RDONLY, 0644));
  ASSERT_TRUE(backing_file.is_valid());

  int test_minor = AddUnusedLoopDevice();
  ASSERT_GE(test_minor, 0);

  std::string device_path = "/dev/loop" + std::to_string(test_minor);
  std::string sys_device_dir = "/sys/block/loop" + std::to_string(test_minor);
  std::string sys_loop_dir = sys_device_dir + "/loop";
  std::string sys_backing_file = sys_loop_dir + "/backing_file";

  fbl::unique_fd loop_fd;
  // Detach the backing file and remove the loop device on every exit path, including early returns
  // from failed assertions. If removal fails, also unlink the leaked /dev/loop<N> node so that
  // subsequent tests scanning /dev do not fail. A successful removal deletes the node itself, and
  // unlinking it afterwards could delete a node re-created by a concurrently running test.
  auto cleanup = fit::defer([&] {
    if (loop_fd.is_valid()) {
      ioctl(loop_fd.get(), LOOP_CLR_FD, 0);
      loop_fd.reset();
    }
    if (RemoveLoopDeviceRetryingOnBusy(test_minor) < 0) {
      unlink(device_path.c_str());
    }
  });

  ASSERT_SUCCESS(access(sys_device_dir.c_str(), F_OK));

  // On an unbound loop device, the `loop` sysfs attribute group directory must not exist.
  EXPECT_THAT(access(sys_loop_dir.c_str(), F_OK), SyscallFailsWithErrno(ENOENT));

  // Attaching a backing file with `bind_request` creates the `loop` attribute group, and detaching
  // it via LOOP_CLR_FD removes the group.
  auto check_lifecycle = [&](unsigned long bind_request, auto bind_arg) {
    loop_fd.reset(open(device_path.c_str(), O_RDWR));
    ASSERT_TRUE(loop_fd.is_valid()) << strerror(errno);
    ASSERT_SUCCESS(ioctl(loop_fd.get(), bind_request, bind_arg));
    EXPECT_THAT(access(sys_loop_dir.c_str(), F_OK), SyscallSucceeds());
    EXPECT_THAT(access(sys_backing_file.c_str(), F_OK), SyscallSucceeds());

    ASSERT_SUCCESS(ioctl(loop_fd.get(), LOOP_CLR_FD, 0));
    loop_fd.reset();
    // On Linux, the backing file and `loop` attribute group are detached on last close after
    // LOOP_CLR_FD, and udev may briefly hold the device open. Starnix does not run udev, so there
    // is nothing to wait for.
    if (!test_helper::IsStarnix()) {
      for (int attempt = 0; attempt < 50 && access(sys_loop_dir.c_str(), F_OK) == 0; ++attempt) {
        usleep(100'000);
      }
    }
    EXPECT_THAT(access(sys_loop_dir.c_str(), F_OK), SyscallFailsWithErrno(ENOENT));
  };

  {
    SCOPED_TRACE("LOOP_SET_FD");
    ASSERT_NO_FATAL_FAILURE(check_lifecycle(LOOP_SET_FD, backing_file.get()));
  }
  {
    SCOPED_TRACE("LOOP_CONFIGURE");
    loop_config config = {
        .fd = static_cast<uint32_t>(backing_file.get()),
        .block_size = 4096,
    };
    ASSERT_NO_FATAL_FAILURE(check_lifecycle(LOOP_CONFIGURE, &config));
  }
}

TEST_F(LoopTest, BlkGetSize64) {
  fbl::unique_fd backing_file(open("data/hello_world.txt", O_RDONLY, 0644));
  ASSERT_TRUE(backing_file.is_valid());

  // Use a large size: 4TiB = 2^42 bytes. This requires more than 32 bits.
  uint64_t expected_size = 4ULL * 1024 * 1024 * 1024 * 1024;
  ActiveLoopDevice device(*this, backing_file.get(), 4096, expected_size);
  ASSERT_TRUE(device.is_valid());

  // Verify that BLKGETSIZE64 correctly reports the size in bytes and is memory safe.
  // We use an array with sentinels to ensure it doesn't stomp adjacent memory and a
  // known pattern to ensure it writes all 8 bytes.
  uint64_t results[3];
  results[0] = 0xAAAAAAAAAAAAAAAAULL;
  results[1] = 0xCCCCCCCCCCCCCCCCULL;
  results[2] = 0xBBBBBBBBBBBBBBBBULL;

  ASSERT_SUCCESS(ioctl(device.fd(), BLKGETSIZE64, &results[1]));

  // 1. Check Units and Precision:
  // If the kernel only wrote 32 bits, 'results[1]' would be 0xCCCCCCCC00000000
  // (on little-endian) and this check would fail.
  EXPECT_EQ(results[1], expected_size);

  // 2. Check Safety: Sentinels must remain untouched.
  EXPECT_EQ(results[0], 0xAAAAAAAAAAAAAAAAULL);
  EXPECT_EQ(results[2], 0xBBBBBBBBBBBBBBBBULL);
}

TEST_F(LoopTest, BlkGetSize) {
  const char* backing_file_path = "data/hello_world.txt";
  fbl::unique_fd backing_file(open(backing_file_path, O_RDONLY));
  ASSERT_TRUE(backing_file.is_valid())
      << "Failed to open backing file " << backing_file_path << " (errno: " << errno << ")";

  // Use a block-aligned size for predictable sector counts: 1024 bytes = 2 sectors.
  uint64_t file_size = 1024;
  uint64_t expected_sectors = file_size / 512;

  // Associate the backing file with the loop device and set the size limit.
  ActiveLoopDevice device(*this, backing_file.get(), 512, file_size);
  ASSERT_TRUE(device.is_valid());

  // Verify that BLKGETSIZE correctly reports the size in 512-byte sectors
  // and is memory safe.
  // We use an array with sentinels to ensure it doesn't stomp adjacent memory.
  unsigned long results[3];
  results[0] = 0xDEADBEEF;
  results[1] = 0;
  results[2] = 0xDEADBEEF;

  ASSERT_SUCCESS(ioctl(device.fd(), BLKGETSIZE, &results[1]));

  // 1. Check Units:
  EXPECT_EQ(results[1], expected_sectors);

  // 2. Check Safety: Sentinels must remain untouched.
  EXPECT_EQ(results[0], 0xDEADBEEF);
  EXPECT_EQ(results[2], 0xDEADBEEF);
}

TEST_F(LoopTest, BlkGetSize_UnitsAndSafety) {
  fbl::unique_fd backing_file(open("data/hello_world.txt", O_RDONLY, 0644));
  ASSERT_TRUE(backing_file.is_valid());

  // Use a large size: 4TiB = 2^42 bytes.
  uint64_t large_size = 4ULL * 1024 * 1024 * 1024 * 1024;
  uint32_t block_size = 4096;
  uint64_t expected_sectors = large_size / block_size;

  ActiveLoopDevice device(*this, backing_file.get(), block_size, large_size);
  ASSERT_TRUE(device.is_valid());

  // Verify that BLKGETSIZE correctly reports the size in blocks
  // (using the device's block_size) and is memory safe.
  // We use an array of 3 longs and pass the middle one.
  unsigned long results[3];
  results[0] = 0xDEADBEEF;
  results[1] = 0xCCCCCCCC;
  results[2] = 0xDEADBEEF;

  // We pass the address of the middle element directly.
  ASSERT_SUCCESS(ioctl(device.fd(), BLKGETSIZE, reinterpret_cast<void*>(&results[1])));

  // 1. Check Units and Truncation:
  // This line should cast to the correct size on 32 and 64 bit
  EXPECT_EQ(results[1], static_cast<unsigned long>(expected_sectors));

  // 2. Check Safety: Sentinels must remain untouched.
  EXPECT_EQ(results[0], 0xDEADBEEF);
  EXPECT_EQ(results[2], 0xDEADBEEF);
}

}  // namespace
