// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include <fcntl.h>
#include <sys/mount.h>
#include <unistd.h>

#include <cerrno>
#include <charconv>
#include <optional>
#include <string>
#include <string_view>

#include <fbl/unique_fd.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

using testing::ContainsRegex;
using testing::IsSupersetOf;

class SysfsKernelTest : public ::testing::Test {
 public:
  void SetUp() override {
    // TODO(https://fxbug.dev/317285180) don't skip on baseline
    // Assume starnix always has /sys/kernel.
    if (!test_helper::IsStarnix() && access("/sys/kernel/wakeup_reasons", F_OK) == -1) {
      GTEST_SKIP() << "/sys/kernel/wakeup_reasons not available, skipping...";
    }
  }
};

TEST_F(SysfsKernelTest, KernelDirectoryContainsExpectedContents) {
  std::vector<std::string> files;
  EXPECT_TRUE(files::ReadDirContents("/sys/kernel", &files));
  EXPECT_THAT(files, IsSupersetOf({"tracing", "wakeup_reasons"}));
}

TEST_F(SysfsKernelTest, WakeupReasonsDirectoryContainsExpectedContents) {
  std::vector<std::string> files;
  EXPECT_TRUE(files::ReadDirContents("/sys/kernel/wakeup_reasons", &files));
  EXPECT_THAT(files, IsSupersetOf({"last_resume_reason", "last_suspend_time"}));
}

TEST_F(SysfsKernelTest, WakeupReasonsDirectoryContainDefaultsLastResumeReason) {
  std::string last_resume_reason_str;
  EXPECT_TRUE(files::ReadFileToString("/sys/kernel/wakeup_reasons/last_resume_reason",
                                      &last_resume_reason_str));
  EXPECT_THAT(last_resume_reason_str, ContainsRegex("^.*\n"));
}

TEST_F(SysfsKernelTest, WakeupReasonsDirectoryContainDefaultsLastSuspendTime) {
  std::string last_suspend_time_str;
  EXPECT_TRUE(files::ReadFileToString("/sys/kernel/wakeup_reasons/last_suspend_time",
                                      &last_suspend_time_str));
  EXPECT_THAT(last_suspend_time_str, ContainsRegex("^[0-9]+ [0-9]+\n"));
}

class TracefsTest : public ::testing::Test {
 protected:
  void SetUp() override {
    if (!test_helper::HasSysAdmin()) {
      GTEST_SKIP() << "Requires CAP_SYS_ADMIN to mount tracefs";
    }
    auto mount_result =
        test_helper::ScopedMount::Mount("tracefs", temp_dir_.path(), "tracefs", MS_NODEV, nullptr);
    if (mount_result.is_error() && mount_result.error_value() == ENODEV) {
      GTEST_SKIP() << "tracefs not supported by kernel";
    }
    ASSERT_THAT(mount_result, SyscallResultIsOk());
    mount_ = std::move(mount_result.value());
  }

  std::string Path(std::string_view relative_path) const {
    return temp_dir_.path() + "/" + std::string(relative_path);
  }

  static fbl::unique_fd OpenForWrite(const std::string& path) {
    return fbl::unique_fd(open(path.c_str(), O_WRONLY | O_TRUNC));
  }

  static bool WriteString(const std::string& path, std::string_view value) {
    return files::WriteFile(path, value.data(), value.size());
  }

  static std::optional<size_t> ReadSizeKb(const std::string& path) {
    std::string content;
    if (!files::ReadFileToString(path, &content)) {
      return std::nullopt;
    }
    size_t value = 0;
    const char* begin = content.data();
    const char* end = begin + content.size();
    auto [ptr, ec] = std::from_chars(begin, end, value);
    if (ec != std::errc() || ptr == begin) {
      return std::nullopt;
    }
    return value;
  }

 private:
  test_helper::ScopedTempDir temp_dir_;
  test_helper::ScopedMount mount_;
};

TEST_F(TracefsTest, BufferSizeKbReadWrite) {
  ASSERT_TRUE(WriteString(Path("tracing_on"), "0\n"));

  auto initial_kb = ReadSizeKb(Path("buffer_size_kb"));
  ASSERT_TRUE(initial_kb.has_value());
  EXPECT_GT(*initial_kb, 0u);

  // Writing a valid size succeeds and updates buffer_size_kb.
  {
    fbl::unique_fd fd = OpenForWrite(Path("buffer_size_kb"));
    ASSERT_TRUE(fd.is_valid()) << strerror(errno);
    EXPECT_THAT(write(fd.get(), "4096\n", 5), SyscallSucceedsWithValue(5));
  }
  std::string content;
  ASSERT_TRUE(files::ReadFileToString(Path("buffer_size_kb"), &content));
  EXPECT_THAT(content, ContainsRegex("^[0-9]+\n$"));
  auto size_4096 = ReadSizeKb(Path("buffer_size_kb"));
  ASSERT_TRUE(size_4096.has_value());
  // Linux divides by usable subbuffer capacity (PAGE_SIZE - 16), reading back 4099 KB on 4K pages.
  EXPECT_GE(*size_4096, 4096u);
  EXPECT_LE(*size_4096, 4160u);

  // Writing a small size succeeds and clamps up to the minimum ring buffer size.
  {
    fbl::unique_fd fd = OpenForWrite(Path("buffer_size_kb"));
    ASSERT_TRUE(fd.is_valid()) << strerror(errno);
    EXPECT_THAT(write(fd.get(), "4\n", 2), SyscallSucceedsWithValue(2));
  }
  auto clamped_kb = ReadSizeKb(Path("buffer_size_kb"));
  ASSERT_TRUE(clamped_kb.has_value());
  EXPECT_GT(*clamped_kb, 4u);
  EXPECT_LE(*clamped_kb, 64u);

  // Writing 0 fails with EINVAL.
  {
    fbl::unique_fd fd = OpenForWrite(Path("buffer_size_kb"));
    ASSERT_TRUE(fd.is_valid()) << strerror(errno);
    EXPECT_THAT(write(fd.get(), "0\n", 2), SyscallFailsWithErrno(EINVAL));
  }

  // Writing non-numeric input fails with EINVAL.
  {
    fbl::unique_fd fd = OpenForWrite(Path("buffer_size_kb"));
    ASSERT_TRUE(fd.is_valid()) << strerror(errno);
    EXPECT_THAT(write(fd.get(), "invalid\n", 8), SyscallFailsWithErrno(EINVAL));
  }

  // Resizing while tracing_on is enabled succeeds.
  ASSERT_TRUE(WriteString(Path("tracing_on"), "1\n"));
  {
    fbl::unique_fd fd = OpenForWrite(Path("buffer_size_kb"));
    ASSERT_TRUE(fd.is_valid()) << strerror(errno);
    EXPECT_THAT(write(fd.get(), "2048\n", 5), SyscallSucceedsWithValue(5));
  }
  auto size_2048 = ReadSizeKb(Path("buffer_size_kb"));
  ASSERT_TRUE(size_2048.has_value());
  EXPECT_GE(*size_2048, 2048u);
  EXPECT_LE(*size_2048, 2112u);

  ASSERT_TRUE(WriteString(Path("tracing_on"), "0\n"));
}
