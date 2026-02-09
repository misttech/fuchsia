// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <dirent.h>
#include <fcntl.h>
#include <ftw.h>
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

 private:
  fbl::unique_fd loop_control_;
};

#define ASSERT_SUCCESS(call) ASSERT_THAT((call), SyscallSucceeds())

TEST_F(LoopTest, ReopeningDevicePreservesOffset) {
  std::string loop_device_path = "/dev/loop" + std::to_string(GetFreeLoopDeviceNumber());
  fbl::unique_fd free_loop_device(open(loop_device_path.c_str(), O_RDONLY, 0644));
  ASSERT_TRUE(free_loop_device.is_valid());

  fbl::unique_fd backing_file(open("data/tests/deps/hello_world.txt", O_RDONLY, 0644));
  ASSERT_TRUE(backing_file.is_valid());

  // Configure an offset that we'll check for after re-opening the device.
  loop_config config = {.fd = static_cast<__u32>(backing_file.get()),
                        .block_size = 4096,
                        .info = {.lo_offset = 4096}};
  ASSERT_SUCCESS(ioctl(free_loop_device.get(), LOOP_CONFIGURE, &config));

  loop_info64 first_observed_info;
  ASSERT_SUCCESS(ioctl(free_loop_device.get(), LOOP_GET_STATUS64, &first_observed_info));

  // Close the loop device fd and reopen it, confirming that the offset and other configuration are
  // the same and preserved even when there are no open files to the device.
  free_loop_device = fbl::unique_fd(open(loop_device_path.c_str(), O_RDONLY, 0644));
  loop_info64 second_observed_info;
  ASSERT_SUCCESS(ioctl(free_loop_device.get(), LOOP_GET_STATUS64, &second_observed_info));
  EXPECT_EQ(first_observed_info.lo_offset, second_observed_info.lo_offset);
}

TEST_F(LoopTest, RemoveLoopDeviceFromKernelDeviceRegistry) {
  int free_loop_device_num = GetFreeLoopDeviceNumber();
  int removed_loop_device_num = RemoveLoopDevice(free_loop_device_num);
  EXPECT_EQ(removed_loop_device_num, free_loop_device_num);
  std::string devfs_path = "/dev/";
  DIR* dir = opendir(devfs_path.c_str());
  ASSERT_TRUE(dir);
  while (struct dirent* entry = readdir(dir)) {
    std::string name = entry->d_name;
    if (name.find("loop") != std::string::npos) {
      std::string device_path = "/dev/" + name;
      int device_fd = open(device_path.c_str(), O_RDONLY);
      ASSERT_GT(device_fd, 0) << "device path is: " << device_path << strerror(errno);
    }
  }
  closedir(dir);
}

TEST_F(LoopTest, BackingFile) {
  auto loop_nr = GetFreeLoopDeviceNumber();
  std::string loop_device_path = "/dev/loop" + std::to_string(loop_nr);
  fbl::unique_fd free_loop_device(open(loop_device_path.c_str(), O_RDONLY, 0644));
  ASSERT_TRUE(free_loop_device.is_valid());

  fbl::unique_fd backing_file(open("data/tests/deps/hello_world.txt", O_RDONLY, 0644));
  ASSERT_TRUE(backing_file.is_valid());

  loop_config config = {
      .fd = static_cast<__u32>(backing_file.get()), .block_size = 4096, .info = {.lo_offset = 0}};
  ASSERT_SUCCESS(ioctl(free_loop_device.get(), LOOP_CONFIGURE, &config));

  loop_info64 first_observed_info;
  ASSERT_SUCCESS(ioctl(free_loop_device.get(), LOOP_GET_STATUS64, &first_observed_info));

  std::string sys_backing_file_path =
      "/sys/block/loop" + std::to_string(loop_nr) + "/loop/backing_file";
  std::ifstream in(sys_backing_file_path);
  ASSERT_TRUE(in.is_open());
  std::stringstream buffer;
  buffer << in.rdbuf();
  // The actual path is prefixed with container namespace, which includes a dynamically
  // generated component identifier.  Since we don't want a change-detector test, just match
  // the suffix.
  ASSERT_TRUE(buffer.str().ends_with("/data/tests/deps/hello_world.txt\n"));
}

TEST_F(LoopTest, BlkGetSize64) {
  auto loop_nr = GetFreeLoopDeviceNumber();
  std::string loop_device_path = "/dev/loop" + std::to_string(loop_nr);
  fbl::unique_fd free_loop_device(open(loop_device_path.c_str(), O_RDONLY, 0644));
  ASSERT_TRUE(free_loop_device.is_valid());

  fbl::unique_fd backing_file(open("data/tests/deps/hello_world.txt", O_RDONLY, 0644));
  ASSERT_TRUE(backing_file.is_valid());

  // Use a large size: 4TiB = 2^42 bytes. This requires more than 32 bits.
  uint64_t expected_size = 4ULL * 1024 * 1024 * 1024 * 1024;
  loop_config config = {.fd = static_cast<__u32>(backing_file.get()),
                        .block_size = 4096,
                        .info = {.lo_offset = 0, .lo_sizelimit = expected_size}};
  ASSERT_SUCCESS(ioctl(free_loop_device.get(), LOOP_CONFIGURE, &config));

  // Verify that BLKGETSIZE64 correctly reports the size in bytes and is memory safe.
  // We use an array with sentinels to ensure it doesn't stomp adjacent memory and a
  // known pattern to ensure it writes all 8 bytes.
  uint64_t results[3];
  results[0] = 0xAAAAAAAAAAAAAAAAULL;
  results[1] = 0xCCCCCCCCCCCCCCCCULL;
  results[2] = 0xBBBBBBBBBBBBBBBBULL;

  ASSERT_SUCCESS(ioctl(free_loop_device.get(), BLKGETSIZE64, &results[1]));

  // 1. Check Units and Precision:
  // If the kernel only wrote 32 bits, 'results[1]' would be 0xCCCCCCCC00000000
  // (on little-endian) and this check would fail.
  EXPECT_EQ(results[1], expected_size);

  // 2. Check Safety: Sentinels must remain untouched.
  EXPECT_EQ(results[0], 0xAAAAAAAAAAAAAAAAULL);
  EXPECT_EQ(results[2], 0xBBBBBBBBBBBBBBBBULL);
}

TEST_F(LoopTest, BlkGetSize) {
  auto loop_nr = GetFreeLoopDeviceNumber();
  ASSERT_GE(loop_nr, 0) << "Failed to find a free loop device.";
  std::string loop_device_path = "/dev/loop" + std::to_string(loop_nr);
  fbl::unique_fd free_loop_device(open(loop_device_path.c_str(), O_RDWR));
  ASSERT_TRUE(free_loop_device.is_valid())
      << "Failed to open " << loop_device_path << " (errno: " << errno << ")";

  const char* backing_file_path = "data/tests/deps/hello_world.txt";
  fbl::unique_fd backing_file(open(backing_file_path, O_RDONLY));
  ASSERT_TRUE(backing_file.is_valid())
      << "Failed to open backing file " << backing_file_path << " (errno: " << errno << ")";

  // Use a block-aligned size for predictable sector counts: 1024 bytes = 2 sectors.
  uint64_t file_size = 1024;
  uint64_t expected_sectors = file_size / 512;

  // Associate the backing file with the loop device and set the size limit.
  loop_config config = {.fd = static_cast<__u32>(backing_file.get()),
                        .block_size = 512,
                        .info = {.lo_offset = 0, .lo_sizelimit = file_size}};
  ASSERT_SUCCESS(ioctl(free_loop_device.get(), LOOP_CONFIGURE, &config))
      << "Failed to LOOP_CONFIGURE";

  // Verify that BLKGETSIZE correctly reports the size in 512-byte sectors
  // and is memory safe.
  // We use an array with sentinels to ensure it doesn't stomp adjacent memory.
  unsigned long results[3];
  results[0] = 0xDEADBEEF;
  results[1] = 0;
  results[2] = 0xDEADBEEF;

  ASSERT_SUCCESS(ioctl(free_loop_device.get(), BLKGETSIZE, &results[1]));

  // 1. Check Units:
  EXPECT_EQ(results[1], expected_sectors);

  // 2. Check Safety: Sentinels must remain untouched.
  EXPECT_EQ(results[0], 0xDEADBEEF);
  EXPECT_EQ(results[2], 0xDEADBEEF);

  // Clean up: Disassociate the file from the loop device
  ASSERT_SUCCESS(ioctl(free_loop_device.get(), LOOP_CLR_FD, 0)) << "Failed to LOOP_CLR_FD";
}

TEST_F(LoopTest, BlkGetSize_UnitsAndSafety) {
  auto loop_nr = GetFreeLoopDeviceNumber();
  std::string loop_device_path = "/dev/loop" + std::to_string(loop_nr);
  fbl::unique_fd free_loop_device(open(loop_device_path.c_str(), O_RDONLY, 0644));
  ASSERT_TRUE(free_loop_device.is_valid());

  fbl::unique_fd backing_file(open("data/tests/deps/hello_world.txt", O_RDONLY, 0644));
  ASSERT_TRUE(backing_file.is_valid());

  // Use a large size: 4TiB = 2^42 bytes.
  uint64_t large_size = 4ULL * 1024 * 1024 * 1024 * 1024;
  uint32_t block_size = 4096;
  uint64_t expected_sectors = large_size / block_size;

  loop_config config = {.fd = static_cast<__u32>(backing_file.get()),
                        .block_size = block_size,
                        .info = {.lo_offset = 0, .lo_sizelimit = large_size}};
  ASSERT_SUCCESS(ioctl(free_loop_device.get(), LOOP_CONFIGURE, &config));

  // Verify that BLKGETSIZE correctly reports the size in blocks
  // (using the device's block_size) and is memory safe.
  // We use an array of 3 longs and pass the middle one.
  unsigned long results[3];
  results[0] = 0xDEADBEEF;
  results[1] = 0xCCCCCCCC;
  results[2] = 0xDEADBEEF;

  // We pass the address of the middle element directly.
  ASSERT_SUCCESS(ioctl(free_loop_device.get(), BLKGETSIZE, reinterpret_cast<void*>(&results[1])));

  // 1. Check Units and Truncation:
  // This line should cast to the correct size on 32 and 64 bit
  EXPECT_EQ(results[1], static_cast<unsigned long>(expected_sectors));

  // 2. Check Safety: Sentinels must remain untouched.
  EXPECT_EQ(results[0], 0xDEADBEEF);
  EXPECT_EQ(results[2], 0xDEADBEEF);
}

}  // namespace
