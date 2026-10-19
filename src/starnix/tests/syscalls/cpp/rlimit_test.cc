// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/sysinfo.h>
#include <sys/time.h>
#include <sys/types.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <string>
#include <string_view>
#include <thread>
#include <vector>

#include <gtest/gtest.h>
#include <linux/capability.h>
#include <linux/futex.h>

#include "src/lib/files/file.h"
#include "src/lib/fxl/strings/string_number_conversions.h"
#include "src/lib/fxl/strings/trim.h"
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

std::atomic<size_t> g_signal_count = 0;

void handle_sigxfsz(int signum) { g_signal_count += 1; }

TEST(SetRLimitTest, ZeroFSizeOnRegularFiles) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    signal(SIGXFSZ, handle_sigxfsz);

    struct rlimit limit = {
        .rlim_cur = 0,
        .rlim_max = 0,
    };

    char path_template[] = "/tmp/XXXXXX";
    char* tmp_path = mkdtemp(path_template);
    ASSERT_NE(tmp_path, nullptr) << "mkdtemp failed" << std::strerror(errno) << '\n';
    std::string file_path = std::string(tmp_path) + "/regular_file";

    ASSERT_EQ(setrlimit(RLIMIT_FSIZE, &limit), 0)
        << "setrlimit failed" << std::strerror(errno) << '\n';

    fbl::unique_fd fd(creat(file_path.c_str(), 0666));
    ASSERT_TRUE(fd.is_valid()) << "failed to create file" << std::strerror(errno) << '\n';

    uint8_t buf[0x20] = {0};
    EXPECT_EQ(write(fd.get(), buf, sizeof(buf)), -1);
    EXPECT_EQ(errno, EFBIG);
    EXPECT_EQ(g_signal_count, 1u);
    signal(SIGXFSZ, SIG_DFL);
    ASSERT_EQ(unlink(file_path.c_str()), 0) << "unlink failed" << std::strerror(errno) << '\n';
    ASSERT_EQ(rmdir(tmp_path), 0) << "rmdir failed" << std::strerror(errno) << '\n';
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(SetRLimitTest, ZeroFSizeOnMemFd) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    signal(SIGXFSZ, handle_sigxfsz);

    struct rlimit limit = {
        .rlim_cur = 0,
        .rlim_max = 0,
    };

    ASSERT_EQ(setrlimit(RLIMIT_FSIZE, &limit), 0)
        << "setrlimit failed" << std::strerror(errno) << '\n';

    fbl::unique_fd fd(test_helper::MemFdCreate("memfd", 0));
    ASSERT_TRUE(fd.is_valid()) << "failed to create file" << std::strerror(errno) << '\n';

    uint8_t buf[0x20] = {0};
    EXPECT_EQ(write(fd.get(), buf, sizeof(buf)), -1);
    EXPECT_EQ(errno, EFBIG);
    EXPECT_EQ(g_signal_count, 1u);
    signal(SIGXFSZ, SIG_DFL);
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

// Test that we can write up to one byte to files.
TEST(SetRLimitTest, OneFSize) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    signal(SIGXFSZ, handle_sigxfsz);

    struct rlimit limit = {
        .rlim_cur = 1,
        .rlim_max = 1,
    };

    ASSERT_EQ(setrlimit(RLIMIT_FSIZE, &limit), 0)
        << "setrlimit failed" << std::strerror(errno) << '\n';

    fbl::unique_fd fd(test_helper::MemFdCreate("memfd", 0));
    ASSERT_TRUE(fd.is_valid()) << "failed to create file" << std::strerror(errno) << '\n';

    uint8_t buf[0x1] = {0};
    EXPECT_EQ(write(fd.get(), buf, sizeof(buf)), 1);
    EXPECT_EQ(g_signal_count, 0u);

    // Next write should fail.
    EXPECT_EQ(write(fd.get(), buf, sizeof(buf)), -1);
    EXPECT_EQ(errno, EFBIG);
    EXPECT_EQ(g_signal_count, 1u);
    signal(SIGXFSZ, SIG_DFL);
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(SetRLimitTest, ZeroFSizeOnPipe) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    signal(SIGXFSZ, handle_sigxfsz);

    struct rlimit limit = {
        .rlim_cur = 0,
        .rlim_max = 0,
    };

    ASSERT_EQ(setrlimit(RLIMIT_FSIZE, &limit), 0)
        << "setrlimit failed" << std::strerror(errno) << '\n';

    int pipefd[2];
    EXPECT_EQ(0, pipe2(pipefd, 0)) << "failed to create pipe" << std::strerror(errno) << '\n';

    uint8_t buf[0x1] = {0};
    ASSERT_EQ(write(pipefd[1], buf, sizeof(buf)), 1);
    EXPECT_EQ(g_signal_count, 0u);

    EXPECT_EQ(read(pipefd[0], buf, sizeof(buf)), 1);
    signal(SIGXFSZ, SIG_DFL);

    close(pipefd[0]);
    close(pipefd[1]);
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(SetRLimitTest, ZeroFSizeOnFIFO) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    signal(SIGXFSZ, handle_sigxfsz);

    struct rlimit limit = {
        .rlim_cur = 0,
        .rlim_max = 0,
    };

    char path_template[] = "/tmp/XXXXXX";
    char* tmp_path = mkdtemp(path_template);
    ASSERT_NE(tmp_path, nullptr) << "mkdtemp failed" << std::strerror(errno) << '\n';
    std::string file_path = std::string(tmp_path) + "/regular_file";

    ASSERT_EQ(setrlimit(RLIMIT_FSIZE, &limit), 0)
        << "setrlimit failed" << std::strerror(errno) << '\n';

    ASSERT_EQ(0, mkfifo(file_path.c_str(), 0666))
        << "failed to create fifo" << std::strerror(errno) << '\n';

    std::thread reader([file_path]() {
      fbl::unique_fd fd(open(file_path.c_str(), O_RDONLY));
      ASSERT_TRUE(fd.is_valid()) << "failed to open file for reading" << std::strerror(errno)
                                 << '\n';

      uint8_t buf[0x1] = {0};
      EXPECT_EQ(read(fd.get(), buf, sizeof(buf)), 1)
          << "read failed" << std::strerror(errno) << '\n';
    });

    fbl::unique_fd fd(open(file_path.c_str(), O_WRONLY));
    ASSERT_TRUE(fd.is_valid()) << "failed to file for writing" << std::strerror(errno) << '\n';

    uint8_t buf[0x1] = {0};
    ASSERT_EQ(write(fd.get(), buf, sizeof(buf)), 1);

    reader.join();

    EXPECT_EQ(g_signal_count, 0u);
    signal(SIGXFSZ, SIG_DFL);

    ASSERT_EQ(unlink(file_path.c_str()), 0) << "unlink failed" << std::strerror(errno) << '\n';
    ASSERT_EQ(rmdir(tmp_path), 0) << "rmdir failed" << std::strerror(errno) << '\n';
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(SetRLimitTest, SpliceToRegularFileWithZeroFSize) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    signal(SIGXFSZ, handle_sigxfsz);

    struct rlimit limit = {
        .rlim_cur = 0,
        .rlim_max = 0,
    };

    ASSERT_EQ(setrlimit(RLIMIT_FSIZE, &limit), 0)
        << "setrlimit failed" << std::strerror(errno) << '\n';

    int pipefd[2];
    ASSERT_EQ(pipe2(pipefd, 0), 0) << "pipe2 failed: " << std::strerror(errno) << '\n';
    fbl::unique_fd pipe_in(pipefd[0]);
    fbl::unique_fd pipe_out(pipefd[1]);

    uint8_t buf[0x20] = {0};
    ASSERT_EQ(write(pipe_out.get(), buf, sizeof(buf)), static_cast<ssize_t>(sizeof(buf)));

    fbl::unique_fd fd(test_helper::MemFdCreate("memfd", 0));
    ASSERT_TRUE(fd.is_valid()) << "failed to create file: " << std::strerror(errno) << '\n';

    EXPECT_EQ(splice(pipe_in.get(), nullptr, fd.get(), nullptr, sizeof(buf), 0), -1);
    EXPECT_EQ(errno, EFBIG);
    EXPECT_EQ(g_signal_count, 1u);

    signal(SIGXFSZ, SIG_DFL);
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(SetRLimitTest, SpliceToRegularFileTruncatedByFSize) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    signal(SIGXFSZ, handle_sigxfsz);

    struct rlimit limit = {
        .rlim_cur = 10,
        .rlim_max = 10,
    };

    ASSERT_EQ(setrlimit(RLIMIT_FSIZE, &limit), 0)
        << "setrlimit failed" << std::strerror(errno) << '\n';

    int pipefd[2];
    ASSERT_EQ(pipe2(pipefd, 0), 0) << "pipe2 failed: " << std::strerror(errno) << '\n';
    fbl::unique_fd pipe_in(pipefd[0]);
    fbl::unique_fd pipe_out(pipefd[1]);

    uint8_t buf[20] = {0};
    ASSERT_EQ(write(pipe_out.get(), buf, sizeof(buf)), 20);

    fbl::unique_fd fd(test_helper::MemFdCreate("memfd", 0));
    ASSERT_TRUE(fd.is_valid()) << "failed to create file: " << std::strerror(errno) << '\n';

    // Splice requested 20 bytes, but RLIMIT_FSIZE is 10. Should splice 10 bytes and deliver
    // SIGXFSZ.
    EXPECT_EQ(splice(pipe_in.get(), nullptr, fd.get(), nullptr, 20, 0), 10);
    EXPECT_EQ(g_signal_count, 1u);

    // Write more data into the pipe so the pipe is not empty.
    ASSERT_EQ(write(pipe_out.get(), buf, 10), 10);

    // Further splice starts at offset 10, which is equal to RLIMIT_FSIZE 10. Should fail with EFBIG
    // and SIGXFSZ.
    EXPECT_EQ(splice(pipe_in.get(), nullptr, fd.get(), nullptr, 10, 0), -1);
    EXPECT_EQ(errno, EFBIG);
    EXPECT_EQ(g_signal_count, 2u);

    signal(SIGXFSZ, SIG_DFL);
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(SetRLimitTest, SpliceToRegularFileWithExplicitOffset) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    signal(SIGXFSZ, handle_sigxfsz);

    struct rlimit limit = {
        .rlim_cur = 10,
        .rlim_max = 10,
    };

    ASSERT_EQ(setrlimit(RLIMIT_FSIZE, &limit), 0)
        << "setrlimit failed" << std::strerror(errno) << '\n';

    int pipefd[2];
    ASSERT_EQ(pipe2(pipefd, 0), 0) << "pipe2 failed: " << std::strerror(errno) << '\n';
    fbl::unique_fd pipe_in(pipefd[0]);
    fbl::unique_fd pipe_out(pipefd[1]);

    uint8_t buf[20] = {0};
    ASSERT_EQ(write(pipe_out.get(), buf, sizeof(buf)), 20);

    fbl::unique_fd fd(test_helper::MemFdCreate("memfd", 0));
    ASSERT_TRUE(fd.is_valid()) << "failed to create file: " << std::strerror(errno) << '\n';

    loff_t off_out = 5;
    // Splice at offset 5 with limit 10: should splice 10 - 5 = 5 bytes, updating off_out to 10 and
    // delivering SIGXFSZ.
    EXPECT_EQ(splice(pipe_in.get(), nullptr, fd.get(), &off_out, 15, 0), 5);
    EXPECT_EQ(off_out, 10);
    EXPECT_EQ(g_signal_count, 1u);

    // Write more data into the pipe so the pipe is not empty.
    ASSERT_EQ(write(pipe_out.get(), buf, 10), 10);

    // Further splice at offset 10 (off_out == 10) >= limit 10. Should fail with EFBIG.
    EXPECT_EQ(splice(pipe_in.get(), nullptr, fd.get(), &off_out, 10, 0), -1);
    EXPECT_EQ(errno, EFBIG);
    EXPECT_EQ(g_signal_count, 2u);

    signal(SIGXFSZ, SIG_DFL);
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(SetRLimitTest, SpliceFromRegularFileToPipeWithZeroFSize) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([&] {
    signal(SIGXFSZ, handle_sigxfsz);

    fbl::unique_fd fd(test_helper::MemFdCreate("memfd", 0));
    ASSERT_TRUE(fd.is_valid()) << "failed to create file: " << std::strerror(errno) << '\n';

    uint8_t buf[10] = {0};
    ASSERT_EQ(write(fd.get(), buf, sizeof(buf)), 10);
    ASSERT_EQ(lseek(fd.get(), 0, SEEK_SET), 0);

    struct rlimit limit = {
        .rlim_cur = 0,
        .rlim_max = 0,
    };

    ASSERT_EQ(setrlimit(RLIMIT_FSIZE, &limit), 0)
        << "setrlimit failed" << std::strerror(errno) << '\n';

    int pipefd[2];
    ASSERT_EQ(pipe2(pipefd, 0), 0) << "pipe2 failed: " << std::strerror(errno) << '\n';
    fbl::unique_fd pipe_in(pipefd[0]);
    fbl::unique_fd pipe_out(pipefd[1]);

    // Splice from regular file to pipe when RLIMIT_FSIZE is 0 should succeed.
    EXPECT_EQ(splice(fd.get(), nullptr, pipe_out.get(), nullptr, 10, 0), 10);
    EXPECT_EQ(g_signal_count, 0u);

    signal(SIGXFSZ, SIG_DFL);
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(SetRLimitTest, RLimitStack) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([] {
    struct rlimit initial = {};
    ASSERT_THAT(getrlimit(RLIMIT_STACK, &initial), SyscallSucceeds());
    EXPECT_EQ(initial.rlim_cur, 8u * 1024 * 1024);
    EXPECT_EQ(initial.rlim_max, RLIM_INFINITY);

    // Raising rlim_cur to RLIM_INFINITY must still cap the execve argument size limit at 6 MiB
    // (3/4 of the default 8 MiB stack limit, per execve(2)).
    struct rlimit inf_limit = {
        .rlim_cur = RLIM_INFINITY,
        .rlim_max = RLIM_INFINITY,
    };
    ASSERT_THAT(setrlimit(RLIMIT_STACK, &inf_limit), SyscallSucceeds());

    // Passing > 6 MiB of arguments (60 * 110 KiB = 6.6 MiB, with each argument under the 32-page
    // per-string limit) must fail with E2BIG before replacing the process image. Pass an existing
    // executable (/proc/self/exe) so execve does not fail earlier with ENOENT.
    std::string large_arg(110 * 1024, 'a');
    std::vector<char*> argv(60, large_arg.data());
    argv.push_back(nullptr);
    char* const envp[] = {nullptr};
    EXPECT_THAT(execve("/proc/self/exe", argv.data(), envp), SyscallFailsWithErrno(E2BIG));

    // Lowering both soft and hard limits succeeds.
    struct rlimit lowered = {
        .rlim_cur = 4 * 1024 * 1024,
        .rlim_max = 16 * 1024 * 1024,
    };
    ASSERT_THAT(setrlimit(RLIMIT_STACK, &lowered), SyscallSucceeds());

    struct rlimit current = {};
    ASSERT_THAT(getrlimit(RLIMIT_STACK, &current), SyscallSucceeds());
    EXPECT_EQ(current.rlim_cur, lowered.rlim_cur);
    EXPECT_EQ(current.rlim_max, lowered.rlim_max);

    // Setting rlim_cur > rlim_max fails with EINVAL.
    struct rlimit invalid = {
        .rlim_cur = 32 * 1024 * 1024,
        .rlim_max = 16 * 1024 * 1024,
    };
    EXPECT_THAT(setrlimit(RLIMIT_STACK, &invalid), SyscallFailsWithErrno(EINVAL));

    // Dropping CAP_SYS_RESOURCE and attempting to raise rlim_max fails with EPERM.
    test_helper::UnsetCapability(CAP_SYS_RESOURCE);
    struct rlimit raised_max = {
        .rlim_cur = 4 * 1024 * 1024,
        .rlim_max = 32 * 1024 * 1024,
    };
    EXPECT_THAT(setrlimit(RLIMIT_STACK, &raised_max), SyscallFailsWithErrno(EPERM));
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

struct RLimitTestCase {
  int resource;
  std::string_view name;  // Short name without RLIMIT_ prefix.
  rlim_t expected_cur;
  rlim_t expected_max;
};

class RLimitDefaultTest : public testing::TestWithParam<RLimitTestCase> {};

TEST_P(RLimitDefaultTest, CurAndMaxDefaults) {
  const RLimitTestCase& test_case = GetParam();

  struct rlimit limit;
  ASSERT_THAT(getrlimit(test_case.resource, &limit), SyscallSucceeds());
  EXPECT_EQ(limit.rlim_cur, test_case.expected_cur);
  EXPECT_EQ(limit.rlim_max, test_case.expected_max);
}

INSTANTIATE_TEST_SUITE_P(
    KernelDefaults, RLimitDefaultTest,
    testing::Values(RLimitTestCase{RLIMIT_CORE, "CORE", 0, RLIM_INFINITY},
                    RLimitTestCase{RLIMIT_CPU, "CPU", RLIM_INFINITY, RLIM_INFINITY},
                    RLimitTestCase{RLIMIT_DATA, "DATA", RLIM_INFINITY, RLIM_INFINITY},
                    RLimitTestCase{RLIMIT_FSIZE, "FSIZE", RLIM_INFINITY, RLIM_INFINITY},
                    RLimitTestCase{RLIMIT_MEMLOCK, "MEMLOCK", 8 * 1024 * 1024, 8 * 1024 * 1024},
                    RLimitTestCase{RLIMIT_NOFILE, "NOFILE", 1024, 524288},
                    RLimitTestCase{RLIMIT_STACK, "STACK", 8 * 1024 * 1024, RLIM_INFINITY},
                    RLimitTestCase{RLIMIT_AS, "AS", RLIM_INFINITY, RLIM_INFINITY},
                    RLimitTestCase{RLIMIT_RSS, "RSS", RLIM_INFINITY, RLIM_INFINITY},
                    RLimitTestCase{RLIMIT_LOCKS, "LOCKS", RLIM_INFINITY, RLIM_INFINITY},
                    RLimitTestCase{RLIMIT_MSGQUEUE, "MSGQUEUE", 819200, 819200},
                    RLimitTestCase{RLIMIT_NICE, "NICE", 0, 0},
                    RLimitTestCase{RLIMIT_RTPRIO, "RTPRIO", 0, 0},
                    RLimitTestCase{RLIMIT_RTTIME, "RTTIME", RLIM_INFINITY, RLIM_INFINITY}),
    [](const testing::TestParamInfo<RLimitTestCase>& info) {
      return std::string(info.param.name);
    });

// Verifies default values of RLIMIT_NPROC and RLIMIT_SIGPENDING.
//
// Per proc_sys_kernel(5), /proc/sys/kernel/threads-max:
//   "This file specifies the system-wide limit on the number of threads
//   (tasks) that can be created on the system."
//   ...
//   "If the thread structures would occupy too much (more than 1/8th) of the
//   available RAM pages, threads-max is reduced accordingly."
//
// On Linux, RLIMIT_NPROC and RLIMIT_SIGPENDING are initialized with equal soft
// and hard limits set to half of /proc/sys/kernel/threads-max.
TEST(RLimitScaledDefaultTest, NProcAndSigpending) {
  struct rlimit nproc;
  ASSERT_THAT(getrlimit(RLIMIT_NPROC, &nproc), SyscallSucceeds());
  EXPECT_EQ(nproc.rlim_cur, nproc.rlim_max);

  struct rlimit sigpending;
  ASSERT_THAT(getrlimit(RLIMIT_SIGPENDING, &sigpending), SyscallSucceeds());
  EXPECT_EQ(sigpending.rlim_cur, sigpending.rlim_max);
  EXPECT_EQ(nproc.rlim_cur, sigpending.rlim_cur);

  std::string threads_max_str;
  ASSERT_TRUE(files::ReadFileToString("/proc/sys/kernel/threads-max", &threads_max_str));
  uint64_t threads_max = 0;
  ASSERT_TRUE(
      fxl::StringToNumberWithError(fxl::TrimString(threads_max_str, " \t\r\n"), &threads_max))
      << "threads-max: " << threads_max_str;
  EXPECT_EQ(nproc.rlim_cur, threads_max / 2);

  struct sysinfo si;
  ASSERT_THAT(sysinfo(&si), SyscallSucceeds());

  // Align with Linux, which budgets 16 KiB per thread stack regardless of the
  // system page size (e.g., 4 pages on 4 KiB kernels or 1 page on 16 KiB
  // kernels), limiting total thread stacks to 1/8th of available RAM and
  // clamping threads-max to [20, FUTEX_TID_MASK].
  constexpr uint64_t kThreadStackSize = 16384;
  constexpr uint64_t kMinThreads = 20;
  constexpr uint64_t kMaxThreads = FUTEX_TID_MASK;

  uint64_t total_ram_bytes = static_cast<uint64_t>(si.totalram) * si.mem_unit;
  uint64_t expected_threads_max =
      std::clamp<uint64_t>(total_ram_bytes / (8 * kThreadStackSize), kMinThreads, kMaxThreads);
  uint64_t expected_limit = expected_threads_max / 2;

  EXPECT_GE(nproc.rlim_cur, 10u);
  // Available RAM in Linux excludes reserved kernel boot pages, so allow a 5% margin.
  EXPECT_GE(nproc.rlim_cur, expected_limit - expected_limit / 20);
  EXPECT_LE(nproc.rlim_cur, expected_limit);
}

}  //  namespace
