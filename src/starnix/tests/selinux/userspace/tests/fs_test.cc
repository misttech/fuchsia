// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <lib/fit/defer.h>
#include <sys/sendfile.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

extern std::string DoPrePolicyLoadWork() { return "fs_test_policy"; }

namespace {
constexpr char kDirLabel[] = "test_u:object_r:test_fs_readdir_dir_t:s0";
constexpr char kSendfileSrcLabel[] = "test_u:object_r:test_fs_sendfile_src_t:s0";

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

TEST(FsTest, ReadWriteOPathFdWithoutPermissionsReturnsEbadf) {
  auto fscreate = ScopedTaskAttrResetter::SetTaskAttr("fscreate", kDirLabel);
  test_helper::ScopedTempDir temp_dir;
  auto src_file = ScopedTempFDWithLabel(kSendfileSrcLabel);
  ASSERT_TRUE(src_file.is_valid());

  fbl::unique_fd dir_fd(open(temp_dir.path().c_str(), O_PATH | O_DIRECTORY));
  ASSERT_THAT(dir_fd.get(), SyscallSucceeds());
  fbl::unique_fd file_fd(open(src_file.name().c_str(), O_PATH));
  ASSERT_THAT(file_fd.get(), SyscallSucceeds());

  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_fs_no_read_t:s0", [&]() {
    char buf[1024] = {};
    EXPECT_THAT(syscall(SYS_getdents64, dir_fd.get(), buf, sizeof(buf)),
                SyscallFailsWithErrno(EBADF));
    EXPECT_THAT(read(dir_fd.get(), buf, sizeof(buf)), SyscallFailsWithErrno(EBADF));
    EXPECT_THAT(write(dir_fd.get(), buf, sizeof(buf)), SyscallFailsWithErrno(EBADF));

    EXPECT_THAT(syscall(SYS_getdents64, file_fd.get(), buf, sizeof(buf)),
                SyscallFailsWithErrno(EBADF));
    EXPECT_THAT(read(file_fd.get(), buf, sizeof(buf)), SyscallFailsWithErrno(EBADF));
    EXPECT_THAT(write(file_fd.get(), buf, sizeof(buf)), SyscallFailsWithErrno(EBADF));
  }));
}

// Verify that readlink() on a directory fails with EINVAL, without checking the read permission
// needed to read symbolic links. `realpath()` relies on this when resolving paths through
// directories that a domain can search but not read.
TEST(FsTest, ReadlinkOnDirectoryFailsWithoutCheck) {
  // Create a directory with the specific label.
  auto fscreate = ScopedTaskAttrResetter::SetTaskAttr("fscreate", kDirLabel);
  test_helper::ScopedTempDir temp_dir;

  auto enforcing = ScopedEnforcement::SetEnforcing();

  // Run as a domain that can search, but not read, the directory.
  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_fs_no_read_t:s0", [&]() {
    char buf[1024];
    EXPECT_THAT(readlink(temp_dir.path().c_str(), buf, sizeof(buf)), SyscallFailsWithErrno(EINVAL));
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

// Verify that attempting a write from a domain denied write access fails with EACCES and
// preserves the SUID and SGID bits on the file.
TEST(FsTest, WriteDeniedPreservesSuid) {
  auto test_file = ScopedTempFDWithLabel(kFallocateFileLabel);
  ASSERT_TRUE(test_file.is_valid());

  ASSERT_THAT(fchmod(test_file.fd(), S_ISUID | S_ISGID | S_IRWXU), SyscallSucceeds());

  struct stat file_stat;
  ASSERT_THAT(fstat(test_file.fd(), &file_stat), SyscallSucceeds());
  ASSERT_NE(file_stat.st_mode & S_ISUID, 0u);
  ASSERT_NE(file_stat.st_mode & S_ISGID, 0u);

  test_helper::UnsetCapability(CAP_FSETID);
  auto restore_cap = fit::defer([]() { test_helper::SetCapabilityEffective(CAP_FSETID); });

  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_fs_no_write_t:s0", [&]() {
    char buf[] = "x";
    EXPECT_THAT(write(test_file.fd(), buf, sizeof(buf)), SyscallFailsWithErrno(EACCES));
  }));

  ASSERT_THAT(fstat(test_file.fd(), &file_stat), SyscallSucceeds());
  EXPECT_NE(file_stat.st_mode & S_ISUID, 0u);
  EXPECT_NE(file_stat.st_mode & S_ISGID, 0u);
}

constexpr char kSendfileDstLabel[] = "test_u:object_r:test_fs_sendfile_dst_t:s0";
constexpr char kPayload[] = "foo";
constexpr size_t kPayloadSize = sizeof(kPayload) - 1;

TEST(FsTest, GetdentsOnFileFdWithoutPermissionsReturnsEnotdir) {
  auto src_file = ScopedTempFDWithLabel(kSendfileSrcLabel);
  ASSERT_TRUE(src_file.is_valid());
  fbl::unique_fd wronly_fd(open(src_file.name().c_str(), O_WRONLY));
  ASSERT_THAT(wronly_fd.get(), SyscallSucceeds());

  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_fs_no_read_t:s0", [&]() {
    char buf[1024] = {};
    EXPECT_THAT(syscall(SYS_getdents64, src_file.fd(), buf, sizeof(buf)),
                SyscallFailsWithErrno(ENOTDIR));
    EXPECT_THAT(syscall(SYS_getdents64, wronly_fd.get(), buf, sizeof(buf)),
                SyscallFailsWithErrno(ENOTDIR));
  }));
}

TEST(FsTest, ReadWriteOnlyFileFdWithoutPermissionsReturnsEbadf) {
  auto src_file = ScopedTempFDWithLabel(kSendfileSrcLabel);
  auto dst_file = ScopedTempFDWithLabel(kSendfileDstLabel);
  ASSERT_TRUE(src_file.is_valid());
  ASSERT_TRUE(dst_file.is_valid());
  ASSERT_THAT(write(src_file.fd(), kPayload, kPayloadSize), SyscallSucceedsWithValue(kPayloadSize));
  ASSERT_THAT(lseek(src_file.fd(), 0, SEEK_SET), SyscallSucceedsWithValue(0));

  fbl::unique_fd wronly_src_fd(open(src_file.name().c_str(), O_WRONLY));
  ASSERT_THAT(wronly_src_fd.get(), SyscallSucceeds());
  fbl::unique_fd rdonly_dst_fd(open(dst_file.name().c_str(), O_RDONLY));
  ASSERT_THAT(rdonly_dst_fd.get(), SyscallSucceeds());

  int pipe_fds[2];
  ASSERT_THAT(pipe(pipe_fds), SyscallSucceeds());
  fbl::unique_fd pipe_in(pipe_fds[0]);
  fbl::unique_fd pipe_out(pipe_fds[1]);
  ASSERT_THAT(write(pipe_out.get(), kPayload, kPayloadSize),
              SyscallSucceedsWithValue(kPayloadSize));

  auto enforcing = ScopedEnforcement::SetEnforcing();

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_fs_no_read_t:s0", [&]() {
    char buf[1024] = {};
    EXPECT_THAT(read(wronly_src_fd.get(), buf, sizeof(buf)), SyscallFailsWithErrno(EBADF));
    EXPECT_THAT(splice(wronly_src_fd.get(), nullptr, pipe_out.get(), nullptr, kPayloadSize, 0),
                SyscallFailsWithErrno(EBADF));
    EXPECT_THAT(splice(src_file.fd(), nullptr, pipe_in.get(), nullptr, kPayloadSize, 0),
                SyscallFailsWithErrno(EBADF));
  }));

  EXPECT_TRUE(RunSubprocessAs("test_u:test_r:test_fs_no_write_t:s0", [&]() {
    EXPECT_THAT(write(rdonly_dst_fd.get(), kPayload, kPayloadSize), SyscallFailsWithErrno(EBADF));
    EXPECT_THAT(splice(pipe_in.get(), nullptr, rdonly_dst_fd.get(), nullptr, kPayloadSize, 0),
                SyscallFailsWithErrno(EBADF));
    EXPECT_THAT(splice(pipe_out.get(), nullptr, dst_file.fd(), nullptr, kPayloadSize, 0),
                SyscallFailsWithErrno(EBADF));
  }));
}

struct FsSecurityTestParams {
  const char* test_domain;
  bool src_readable;
  bool dst_writable;
};

struct ScopedFifoWithLabel {
  test_helper::ScopedTempDir dir;
  fbl::unique_fd read_fd;
  fbl::unique_fd write_fd;

  static ScopedFifoWithLabel Create(std::string_view label) {
    ScopedFifoWithLabel fifo;
    std::string fifo_path = fifo.dir.path() + "/fifo";
    {
      auto fscreate = ScopedTaskAttrResetter::SetTaskAttr("fscreate", label);
      EXPECT_THAT(mkfifo(fifo_path.c_str(), 0600), SyscallSucceeds());
    }
    fifo.read_fd = fbl::unique_fd(open(fifo_path.c_str(), O_RDONLY | O_NONBLOCK));
    EXPECT_TRUE(fifo.read_fd.is_valid());
    fifo.write_fd = fbl::unique_fd(open(fifo_path.c_str(), O_WRONLY | O_NONBLOCK));
    EXPECT_TRUE(fifo.write_fd.is_valid());
    return fifo;
  }
};

class FsSecurityTest : public ::testing::TestWithParam<FsSecurityTestParams> {};

// Verify that sendfile enforces SELinux read/write permissions.
TEST_P(FsSecurityTest, Sendfile) {
  const auto& params = GetParam();
  auto src_file = ScopedTempFDWithLabel(kSendfileSrcLabel);
  auto dst_file = ScopedTempFDWithLabel(kSendfileDstLabel);
  ASSERT_TRUE(src_file.is_valid());
  ASSERT_TRUE(dst_file.is_valid());

  // Write some data to the src file.
  ASSERT_THAT(write(src_file.fd(), kPayload, kPayloadSize), SyscallSucceedsWithValue(kPayloadSize));
  ASSERT_THAT(lseek(src_file.fd(), 0, SEEK_SET), SyscallSucceedsWithValue(0));

  auto enforcing = ScopedEnforcement::SetEnforcing();
  std::string context = MakeTestSecurityContext(params.test_domain);

  EXPECT_TRUE(RunSubprocessAs(context, [&]() {
    if (params.src_readable && params.dst_writable) {
      EXPECT_THAT(sendfile(dst_file.fd(), src_file.fd(), nullptr, kPayloadSize), SyscallSucceeds());
    } else {
      EXPECT_THAT(sendfile(dst_file.fd(), src_file.fd(), nullptr, kPayloadSize),
                  SyscallFailsWithErrno(EACCES));
    }
  }));
}

// Verify that splice from a src file to a pipe enforces SELinux read permissions.
TEST_P(FsSecurityTest, SpliceFromSrc) {
  const auto& params = GetParam();
  auto src_file = ScopedTempFDWithLabel(kSendfileSrcLabel);
  ASSERT_TRUE(src_file.is_valid());
  ASSERT_THAT(write(src_file.fd(), kPayload, kPayloadSize), SyscallSucceedsWithValue(kPayloadSize));
  ASSERT_THAT(lseek(src_file.fd(), 0, SEEK_SET), SyscallSucceedsWithValue(0));

  int pipe_fds[2];
  ASSERT_THAT(pipe(pipe_fds), SyscallSucceeds());
  fbl::unique_fd pipe_in(pipe_fds[0]);
  fbl::unique_fd pipe_out(pipe_fds[1]);

  auto enforcing = ScopedEnforcement::SetEnforcing();
  std::string context = MakeTestSecurityContext(params.test_domain);

  EXPECT_TRUE(RunSubprocessAs(context, [&]() {
    if (params.src_readable) {
      EXPECT_THAT(splice(src_file.fd(), nullptr, pipe_out.get(), nullptr, kPayloadSize, 0),
                  SyscallSucceeds());
    } else {
      EXPECT_THAT(splice(src_file.fd(), nullptr, pipe_out.get(), nullptr, kPayloadSize, 0),
                  SyscallFailsWithErrno(EACCES));
    }
  }));
}

// Verify that splice from a pipe to a dst file enforces SELinux write permissions.
TEST_P(FsSecurityTest, SpliceToDst) {
  const auto& params = GetParam();
  auto dst_file = ScopedTempFDWithLabel(kSendfileDstLabel);
  ASSERT_TRUE(dst_file.is_valid());

  int pipe_fds[2];
  ASSERT_THAT(pipe(pipe_fds), SyscallSucceeds());
  fbl::unique_fd pipe_in(pipe_fds[0]);
  fbl::unique_fd pipe_out(pipe_fds[1]);
  ASSERT_THAT(write(pipe_out.get(), kPayload, kPayloadSize),
              SyscallSucceedsWithValue(kPayloadSize));

  auto enforcing = ScopedEnforcement::SetEnforcing();
  std::string context = MakeTestSecurityContext(params.test_domain);

  EXPECT_TRUE(RunSubprocessAs(context, [&]() {
    if (params.dst_writable) {
      EXPECT_THAT(splice(pipe_in.get(), nullptr, dst_file.fd(), nullptr, kPayloadSize, 0),
                  SyscallSucceeds());
    } else {
      EXPECT_THAT(splice(pipe_in.get(), nullptr, dst_file.fd(), nullptr, kPayloadSize, 0),
                  SyscallFailsWithErrno(EACCES));
    }
  }));
}

// TODO(https://fxbug.dev/572276669): Verify whether pipe-to-pipe splice, tee, and vmsplice should
// check SELinux read/write permissions on the pipe endpoints.
TEST_P(FsSecurityTest, SplicePipeToPipe) {
  const auto& params = GetParam();
  auto src_fifo = ScopedFifoWithLabel::Create(kSendfileSrcLabel);
  auto dst_fifo = ScopedFifoWithLabel::Create(kSendfileDstLabel);
  ASSERT_TRUE(src_fifo.read_fd.is_valid() && src_fifo.write_fd.is_valid());
  ASSERT_TRUE(dst_fifo.read_fd.is_valid() && dst_fifo.write_fd.is_valid());

  ASSERT_THAT(write(src_fifo.write_fd.get(), kPayload, kPayloadSize),
              SyscallSucceedsWithValue(kPayloadSize));

  auto enforcing = ScopedEnforcement::SetEnforcing();
  std::string context = MakeTestSecurityContext(params.test_domain);

  EXPECT_TRUE(RunSubprocessAs(context, [&]() {
    EXPECT_THAT(
        splice(src_fifo.read_fd.get(), nullptr, dst_fifo.write_fd.get(), nullptr, kPayloadSize, 0),
        SyscallSucceedsWithValue(kPayloadSize));
  }));
}

// Verify that tee does not check SELinux read/write permissions on the pipe endpoints.
TEST_P(FsSecurityTest, Tee) {
  const auto& params = GetParam();
  auto src_fifo = ScopedFifoWithLabel::Create(kSendfileSrcLabel);
  auto dst_fifo = ScopedFifoWithLabel::Create(kSendfileDstLabel);
  ASSERT_TRUE(src_fifo.read_fd.is_valid() && src_fifo.write_fd.is_valid());
  ASSERT_TRUE(dst_fifo.read_fd.is_valid() && dst_fifo.write_fd.is_valid());

  ASSERT_THAT(write(src_fifo.write_fd.get(), kPayload, kPayloadSize),
              SyscallSucceedsWithValue(kPayloadSize));

  auto enforcing = ScopedEnforcement::SetEnforcing();
  std::string context = MakeTestSecurityContext(params.test_domain);

  EXPECT_TRUE(RunSubprocessAs(context, [&]() {
    EXPECT_THAT(tee(src_fifo.read_fd.get(), dst_fifo.write_fd.get(), kPayloadSize, 0),
                SyscallSucceedsWithValue(kPayloadSize));
  }));
}

// Verify that vmsplice writing into a pipe does not check SELinux write permission on the pipe.
TEST_P(FsSecurityTest, VmspliceToPipe) {
  const auto& params = GetParam();
  auto dst_fifo = ScopedFifoWithLabel::Create(kSendfileDstLabel);
  ASSERT_TRUE(dst_fifo.read_fd.is_valid() && dst_fifo.write_fd.is_valid());

  char buf[] = "foo";
  struct iovec iov = {.iov_base = buf, .iov_len = sizeof(buf) - 1};

  auto enforcing = ScopedEnforcement::SetEnforcing();
  std::string context = MakeTestSecurityContext(params.test_domain);

  EXPECT_TRUE(RunSubprocessAs(context, [&]() {
    EXPECT_THAT(vmsplice(dst_fifo.write_fd.get(), &iov, 1, 0),
                SyscallSucceedsWithValue(kPayloadSize));
  }));
}

// Verify that vmsplice reading from a pipe does not check SELinux read permission on the pipe.
TEST_P(FsSecurityTest, VmspliceFromPipe) {
  const auto& params = GetParam();
  auto src_fifo = ScopedFifoWithLabel::Create(kSendfileSrcLabel);
  ASSERT_TRUE(src_fifo.read_fd.is_valid() && src_fifo.write_fd.is_valid());

  ASSERT_THAT(write(src_fifo.write_fd.get(), kPayload, kPayloadSize),
              SyscallSucceedsWithValue(kPayloadSize));

  char buf[16] = {};
  struct iovec iov = {.iov_base = buf, .iov_len = kPayloadSize};

  auto enforcing = ScopedEnforcement::SetEnforcing();
  std::string context = MakeTestSecurityContext(params.test_domain);

  EXPECT_TRUE(RunSubprocessAs(context, [&]() {
    EXPECT_THAT(vmsplice(src_fifo.read_fd.get(), &iov, 1, 0),
                SyscallSucceedsWithValue(kPayloadSize));
  }));
}

INSTANTIATE_TEST_SUITE_P(FsSecurity, FsSecurityTest,
                         ::testing::Values(
                             FsSecurityTestParams{
                                 .test_domain = "test_fs_t",
                                 .src_readable = true,
                                 .dst_writable = true,
                             },
                             FsSecurityTestParams{
                                 .test_domain = "test_fs_no_read_t",
                                 .src_readable = false,
                                 .dst_writable = true,
                             },
                             FsSecurityTestParams{
                                 .test_domain = "test_fs_no_write_t",
                                 .src_readable = true,
                                 .dst_writable = false,
                             }),
                         [](const ::testing::TestParamInfo<FsSecurityTest::ParamType>& info) {
                           return info.param.test_domain;
                         });

}  // namespace
