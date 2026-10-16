// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <lib/fit/defer.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/personality.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <unistd.h>

#include <optional>
#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/io_uring.h>

#include "src/lib/files/file.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

TEST(PersonalityTest, UnameMachineName) {
  struct utsname name;

  int orig_pers = personality(0xffffffff);
  ASSERT_NE(orig_pers, -1);

  // Set personality to PER_LINUX
  int prev = personality(PER_LINUX);
  ASSERT_NE(prev, -1);

  ASSERT_EQ(uname(&name), 0);
#if defined(__aarch64__) || defined(__arm__)
  EXPECT_STREQ(name.machine, "aarch64");
#elif defined(__x86_64__) || defined(__i386__)
  EXPECT_STREQ(name.machine, "x86_64");
#elif defined(__riscv)
  EXPECT_STREQ(name.machine, "riscv64");
#endif

  // Set personality to PER_LINUX32
  prev = personality(PER_LINUX32);
  ASSERT_NE(prev, -1);

  ASSERT_EQ(uname(&name), 0);
#if defined(__aarch64__) || defined(__arm__)
  EXPECT_STREQ(name.machine, "armv8l");
#elif defined(__x86_64__) || defined(__i386__)
  EXPECT_STREQ(name.machine, "i686");
#elif defined(__riscv)
  EXPECT_STREQ(name.machine, "riscv32");
#endif

  // Restore
  personality(orig_pers);
}

namespace {

// Returns the permissions in /proc/self/maps of the mapping containing `addr`, or std::nullopt if
// there is no such mapping.
std::optional<std::string> GetMappingPerms(const void* addr) {
  std::string maps;
  if (!files::ReadFileToString("/proc/self/maps", &maps)) {
    ADD_FAILURE() << "failed to read /proc/self/maps";
    return std::nullopt;
  }
  auto mapping = test_helper::find_memory_mapping(reinterpret_cast<uintptr_t>(addr), maps);
  if (!mapping) {
    return std::nullopt;
  }
  return mapping->perms;
}

// Sets the `READ_IMPLIES_EXEC` personality flag until the returned object is destroyed.
auto ScopedReadImpliesExec() {
  int orig_pers = personality(0xffffffff);
  EXPECT_NE(orig_pers, -1);
  EXPECT_NE(personality(orig_pers | READ_IMPLIES_EXEC), -1);
  return fit::defer([orig_pers]() { personality(orig_pers); });
}

// Mounts a tmpfs with `MS_NOEXEC` on `temp_dir`, until the returned object is destroyed.
auto ScopedNoexecMount(const test_helper::ScopedTempDir& temp_dir) {
  EXPECT_EQ(mount(nullptr, temp_dir.path().c_str(), "tmpfs", MS_NOEXEC, nullptr), 0)
      << strerror(errno);
  return fit::defer([&temp_dir]() { umount2(temp_dir.path().c_str(), MNT_DETACH); });
}

// Creates a file of one page in `temp_dir`.
fbl::unique_fd CreateFile(const test_helper::ScopedTempDir& temp_dir) {
  std::string path = temp_dir.path() + "/file";
  fbl::unique_fd fd(open(path.c_str(), O_RDWR | O_CREAT, 0755));
  EXPECT_TRUE(fd.is_valid()) << strerror(errno);
  EXPECT_EQ(ftruncate(fd.get(), sysconf(_SC_PAGE_SIZE)), 0) << strerror(errno);
  return fd;
}

}  // namespace

TEST(PersonalityTest, ReadImpliesExecMmap) {
  const size_t page_size = sysconf(_SC_PAGE_SIZE);
  {
    auto restore = ScopedReadImpliesExec();

    auto mapping = test_helper::ScopedMMap::MMap(nullptr, page_size, PROT_READ,
                                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_THAT(mapping, SyscallResultIsOk());
    EXPECT_EQ(GetMappingPerms(mapping->mapping()), "r-xp");

    // `PROT_EXEC` is only implied by `PROT_READ`.
    auto none_mapping = test_helper::ScopedMMap::MMap(nullptr, page_size, PROT_NONE,
                                                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_THAT(none_mapping, SyscallResultIsOk());
    EXPECT_EQ(GetMappingPerms(none_mapping->mapping()), "---p");
  }

  // Without the flag, `PROT_READ` doesn't imply `PROT_EXEC`.
  auto mapping = test_helper::ScopedMMap::MMap(nullptr, page_size, PROT_READ,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());
  EXPECT_EQ(GetMappingPerms(mapping->mapping()), "r--p");
}

TEST(PersonalityTest, ReadImpliesExecMprotect) {
  const size_t page_size = sysconf(_SC_PAGE_SIZE);
  auto mapping = test_helper::ScopedMMap::MMap(nullptr, page_size, PROT_NONE,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());

  {
    auto restore = ScopedReadImpliesExec();
    ASSERT_THAT(mprotect(mapping->mapping(), page_size, PROT_READ | PROT_WRITE), SyscallSucceeds());
    EXPECT_EQ(GetMappingPerms(mapping->mapping()), "rwxp");
  }

  ASSERT_THAT(mprotect(mapping->mapping(), page_size, PROT_READ | PROT_WRITE), SyscallSucceeds());
  EXPECT_EQ(GetMappingPerms(mapping->mapping()), "rw-p");
}

TEST(PersonalityTest, ReadImpliesExecMprotectFile) {
  const size_t page_size = sysconf(_SC_PAGE_SIZE);
  test_helper::ScopedTempDir temp_dir;
  struct statvfs buf = {};
  if (statvfs(temp_dir.path().c_str(), &buf) == 0 && (buf.f_flag & ST_NOEXEC) != 0) {
    GTEST_SKIP() << "The temporary directory is on a noexec mount.";
  }
  fbl::unique_fd fd = CreateFile(temp_dir);
  ASSERT_TRUE(fd.is_valid());

  // The file is initially mapped without `PROT_EXEC`, so its backing memory is obtained without
  // execute rights.
  auto mapping =
      test_helper::ScopedMMap::MMap(nullptr, page_size, PROT_NONE, MAP_PRIVATE, fd.get(), 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());

  auto restore = ScopedReadImpliesExec();
  ASSERT_THAT(mprotect(mapping->mapping(), page_size, PROT_READ), SyscallSucceeds());
  EXPECT_EQ(GetMappingPerms(mapping->mapping()), "r-xp");
}

TEST(PersonalityTest, ReadImpliesExecMmapNoexecFile) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Test uses mount() but running without sysadmin. Skipping.";
  }
  const size_t page_size = sysconf(_SC_PAGE_SIZE);
  test_helper::ScopedTempDir temp_dir;
  auto unmount = ScopedNoexecMount(temp_dir);
  fbl::unique_fd fd = CreateFile(temp_dir);
  ASSERT_TRUE(fd.is_valid());

  auto restore = ScopedReadImpliesExec();
  // Files that can't be executed are still mapped, without `PROT_EXEC`.
  auto mapping =
      test_helper::ScopedMMap::MMap(nullptr, page_size, PROT_READ, MAP_PRIVATE, fd.get(), 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());
  EXPECT_EQ(GetMappingPerms(mapping->mapping()), "r--p");
}

TEST(PersonalityTest, ReadImpliesExecMmapIoUring) {
  io_uring_params params = {};
  fbl::unique_fd ring_fd(static_cast<int>(syscall(__NR_io_uring_setup, 1, &params)));
  if (!ring_fd.is_valid()) {
    GTEST_SKIP() << "io_uring is not supported: " << strerror(errno);
  }
  size_t length = params.sq_off.array + params.sq_entries * sizeof(uint32_t);

  auto restore = ScopedReadImpliesExec();
  auto mapping = test_helper::ScopedMMap::MMap(nullptr, length, PROT_READ, MAP_SHARED,
                                               ring_fd.get(), IORING_OFF_SQ_RING);
  ASSERT_THAT(mapping, SyscallResultIsOk());
  std::optional<std::string> perms = GetMappingPerms(mapping->mapping());
  if (test_helper::IsStarnix()) {
    // Starnix doesn't make the memory of the rings, which it shares with the process, executable.
    // It maps the rings without `PROT_EXEC` instead.
    EXPECT_EQ(perms, "r--s");
  } else {
    // Depending on the kernel version, Linux maps the rings with or without `PROT_EXEC`.
    EXPECT_TRUE(perms == "r-xs" || perms == "r--s") << perms.value_or("no mapping");
  }
}

TEST(PersonalityTest, ReadImpliesExecMprotectNoexecFile) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Test uses mount() but running without sysadmin. Skipping.";
  }
  const size_t page_size = sysconf(_SC_PAGE_SIZE);
  test_helper::ScopedTempDir temp_dir;
  auto unmount = ScopedNoexecMount(temp_dir);
  fbl::unique_fd fd = CreateFile(temp_dir);
  ASSERT_TRUE(fd.is_valid());

  auto mapping =
      test_helper::ScopedMMap::MMap(nullptr, page_size, PROT_NONE, MAP_PRIVATE, fd.get(), 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());

  auto restore = ScopedReadImpliesExec();
  // Mappings that can't be executable are made readable, without `PROT_EXEC`.
  ASSERT_THAT(mprotect(mapping->mapping(), page_size, PROT_READ), SyscallSucceeds());
  EXPECT_EQ(GetMappingPerms(mapping->mapping()), "r--p");
}

TEST(PersonalityTest, ReadImpliesExecMprotectAfterFork) {
  const size_t page_size = sysconf(_SC_PAGE_SIZE);
  auto mapping = test_helper::ScopedMMap::MMap(nullptr, page_size, PROT_READ,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());

  // The mappings of a forked child may not be mapped yet when it changes their protection.
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    auto restore = ScopedReadImpliesExec();
    ASSERT_THAT(mprotect(mapping->mapping(), page_size, PROT_READ), SyscallSucceeds());
    EXPECT_EQ(GetMappingPerms(mapping->mapping()), "r-xp");
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(PersonalityTest, ReadImpliesExecMprotectMixedMappings) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "Test uses mount() but running without sysadmin. Skipping.";
  }
  const size_t page_size = sysconf(_SC_PAGE_SIZE);
  test_helper::ScopedTempDir temp_dir;
  auto unmount = ScopedNoexecMount(temp_dir);
  fbl::unique_fd fd = CreateFile(temp_dir);
  ASSERT_TRUE(fd.is_valid());

  // An anonymous mapping followed by a mapping of a file that can't be executed.
  auto mapping = test_helper::ScopedMMap::MMap(nullptr, page_size * 2, PROT_NONE,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());
  char* anonymous_page = static_cast<char*>(mapping->mapping());
  char* file_page = anonymous_page + page_size;
  ASSERT_NE(mmap(file_page, page_size, PROT_NONE, MAP_PRIVATE | MAP_FIXED, fd.get(), 0), MAP_FAILED)
      << strerror(errno);

  // `PROT_EXEC` is only implied for the mapping that can be executable.
  auto restore = ScopedReadImpliesExec();
  ASSERT_THAT(mprotect(anonymous_page, page_size * 2, PROT_READ), SyscallSucceeds());
  EXPECT_EQ(GetMappingPerms(anonymous_page), "r-xp");
  EXPECT_EQ(GetMappingPerms(file_page), "r--p");
}
