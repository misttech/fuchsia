// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/syscall.h>
#include <unistd.h>

#include <cstdint>
#include <memory>
#include <string>

#include <gtest/gtest.h>
#include <linux/hw_breakpoint.h> /* Definition of HW_* constants */
#include <linux/perf_event.h>    /* Definition of PERF_* constants */

#include "fbl/unique_fd.h"
#include "gmock/gmock.h"
#include "src/lib/files/file.h"
#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

extern std::string DoPrePolicyLoadWork() { return "perf_event.pp"; }

namespace {

constexpr int kAllTasksPid = -1;
constexpr int kCurrentTaskPid = 0;

int perf_event_open(struct perf_event_attr* attr, pid_t pid, int cpu) {
  return (int)syscall(__NR_perf_event_open, attr, pid, cpu, /*group_fd=*/-1, /*flags=*/0);
}

class PerfEventTest : public ::testing::Test {};

std::unique_ptr<struct perf_event_attr> GetPerfEventAttr(uint32_t type, uint64_t config,
                                                         bool exclude_kernel) {
  auto pe = std::make_unique<struct perf_event_attr>();

  // Set parameters for the event we want to measure:
  pe->type = type;
  pe->size = sizeof(struct perf_event_attr);
  pe->config = config;

  // Include or exclude count for events that happen in kernel space.
  // When `0` the `kernel` permission is always checked.
  pe->exclude_kernel = exclude_kernel;

  return pe;
}

class OpenTypeHardware : public PerfEventTest,
                         public testing::WithParamInterface<std::tuple<int, int>> {};

TEST_P(OpenTypeHardware, OpenEventsAllPermissions) {
  /// When all permissions are provided, perf_event_open with type Hardware succeeds.
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto& [config, pid] = GetParam();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_all_permissions_t:s0", [&] {
    auto pe = GetPerfEventAttr(PERF_TYPE_HARDWARE, config, /*exclude_kernel=*/false);
    fbl::unique_fd fd(perf_event_open(pe.get(), pid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

TEST_P(OpenTypeHardware, OpenEventsNoKernel) {
  /// When exclude_kernel is 1, perf_event_open doesn't require the kernel permission.
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto& [config, pid] = GetParam();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_no_kernel_t:s0", [&] {
    auto pe = GetPerfEventAttr(PERF_TYPE_HARDWARE, config, /*exclude_kernel=*/true);
    fbl::unique_fd fd(perf_event_open(pe.get(), pid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

TEST_P(OpenTypeHardware, OpenEventsNoKernelFails) {
  /// When exclude_kernel is 0, and kernel permission is missing, expect a failure.
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto& [config, pid] = GetParam();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_no_kernel_t:s0", [&] {
    auto pe = GetPerfEventAttr(PERF_TYPE_HARDWARE, config,
                               /*exclude_kernel=*/false);
    EXPECT_THAT(perf_event_open(pe.get(), pid, 0 /* this CPU */), SyscallFailsWithErrno(EACCES));
  }));
}

// Missing values: PERF_COUNT_HW_BUS_CYCLES, PERF_COUNT_HW_REF_CPU_CYCLES
// Because those fail with "No such file or directory".
const auto kPerfEventOpenHardware = ::testing::Combine(
    ::testing::Values(PERF_COUNT_HW_CPU_CYCLES, PERF_COUNT_HW_INSTRUCTIONS,
                      PERF_COUNT_HW_CACHE_REFERENCES, PERF_COUNT_HW_CACHE_MISSES,
                      PERF_COUNT_HW_BRANCH_INSTRUCTIONS, PERF_COUNT_HW_BRANCH_MISSES,
                      PERF_COUNT_HW_STALLED_CYCLES_FRONTEND, PERF_COUNT_HW_STALLED_CYCLES_BACKEND),
    ::testing::Values(kAllTasksPid, kCurrentTaskPid));
INSTANTIATE_TEST_SUITE_P(PerfEventTest, OpenTypeHardware, kPerfEventOpenHardware);

class OpenTypeSoftware : public PerfEventTest,
                         public testing::WithParamInterface<std::tuple<int, int>> {};

TEST_P(OpenTypeSoftware, OpenEventsAllPermissions) {
  /// When all permissions are provided, perf_event_open with type Hardware succeeds.
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto& [config, pid] = GetParam();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_all_permissions_t:s0", [&] {
    auto pe = GetPerfEventAttr(PERF_TYPE_SOFTWARE, config, /*exclude_kernel=*/false);
    fbl::unique_fd fd(perf_event_open(pe.get(), pid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

TEST_P(OpenTypeSoftware, OpenEventsNoKernel) {
  /// When exclude_kernel is 1, perf_event_open doesn't require the kernel permission.
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto& [config, pid] = GetParam();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_no_kernel_t:s0", [&] {
    auto pe = GetPerfEventAttr(PERF_TYPE_SOFTWARE, config, /*exclude_kernel=*/true);
    fbl::unique_fd fd(perf_event_open(pe.get(), pid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

TEST_P(OpenTypeSoftware, OpenEventsNoKernelFails) {
  /// When exclude_kernel is 0, and kernel permission is missing, expect a failure.
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto& [config, pid] = GetParam();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_no_kernel_t:s0", [&] {
    auto pe = GetPerfEventAttr(PERF_TYPE_SOFTWARE, config, /*exclude_kernel=*/false);
    EXPECT_THAT(perf_event_open(pe.get(), pid, 0 /* this CPU */), SyscallFailsWithErrno(EACCES));
  }));
}

// All correct values of config for PERF_EVENT_TYPE_SOFTWARE.
const auto kPerfEventOpenSoftware = ::testing::Combine(
    ::testing::Range(0, (int)PERF_COUNT_SW_MAX), ::testing::Values(kAllTasksPid, kCurrentTaskPid));
INSTANTIATE_TEST_SUITE_P(PerfEventTest, OpenTypeSoftware, kPerfEventOpenSoftware);

class OpenTypeHWCacheOrTracepoint : public PerfEventTest,
                                    public testing::WithParamInterface<uint32_t> {};

TEST_P(OpenTypeHWCacheOrTracepoint, OpenEventsAllPermissions) {
  // perf_event_open succeeds over different values of `pid` and `exclude_kernel`.
  const auto perf_type = OpenTypeHWCacheOrTracepoint::GetParam();
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_all_permissions_t:s0", [&] {
    // All `pid`s, exclude_kernel = 0.
    auto pe = GetPerfEventAttr(perf_type, 1, /*exclude_kernel=*/false);
    fbl::unique_fd fd(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // All `pid`s, exclude_kernel = 1.
    pe = GetPerfEventAttr(perf_type, 1, /*exclude_kernel=*/true);
    fd = fbl::unique_fd(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Current pid, exclude_kernel = 0.
    pe = GetPerfEventAttr(perf_type, 1, /*exclude_kernel=*/false);
    fd = fbl::unique_fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Current pid, exclude_kernel = 1.
    pe = GetPerfEventAttr(perf_type, 1, /*exclude_kernel=*/true);
    fd = fbl::unique_fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

TEST_P(OpenTypeHWCacheOrTracepoint, OpenEventsNoKernel) {
  // Without the kernel permission, perf_event_open only succeeds when `exclude_kernel` == 1.
  const auto perf_type = OpenTypeHWCacheOrTracepoint::GetParam();
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_no_kernel_t:s0", [&] {
    // All `pid`s, exclude_kernel = 0.
    auto pe = GetPerfEventAttr(perf_type, 1, /*exclude_kernel=*/false);
    EXPECT_THAT(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // All `pid`s, exclude_kernel = 1.
    pe = GetPerfEventAttr(perf_type, 1, /*exclude_kernel=*/true);
    fbl::unique_fd fd(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Current pid, exclude_kernel = 0.
    pe = GetPerfEventAttr(perf_type, 1, /*exclude_kernel=*/false);
    EXPECT_THAT(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // Current pid, exclude_kernel = 1.
    pe = GetPerfEventAttr(perf_type, 1, /*exclude_kernel=*/true);
    fd = fbl::unique_fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

TEST_P(OpenTypeHWCacheOrTracepoint, OpenEventsNoCpu) {
  // Without the CPU permission, perf_event_open only succeeds when `pid` == 0.
  const auto perf_type = OpenTypeHWCacheOrTracepoint::GetParam();
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_no_cpu_t:s0", [&] {
    // All `pid`s, exclude_kernel = 0.
    auto pe = GetPerfEventAttr(perf_type, 1, /*exclude_kernel=*/false);
    EXPECT_THAT(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // All `pid`s, exclude_kernel = 1.
    pe = GetPerfEventAttr(perf_type, 1, /*exclude_kernel=*/true);
    EXPECT_THAT(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // Current pid, exclude_kernel = 0.
    pe = GetPerfEventAttr(perf_type, 1, /*exclude_kernel=*/false);
    fbl::unique_fd fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Current pid, exclude_kernel = 1.
    pe = GetPerfEventAttr(perf_type, 1, /*exclude_kernel=*/true);
    fd = fbl::unique_fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

const auto kPerfEventOpenHWCacheOrTracepoint =
    ::testing::Values(PERF_TYPE_HW_CACHE, PERF_TYPE_TRACEPOINT);
INSTANTIATE_TEST_SUITE_P(PerfEventTest, OpenTypeHWCacheOrTracepoint,
                         kPerfEventOpenHWCacheOrTracepoint);

TEST(PerfEventTest, OpenEventsNoTracepoint) {
  // perf_event_open fails for PERF_TYPE_TRACEPOINT without the tracepoint permission.
  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_no_tracepoint_t:s0", [&] {
    // All `pid`s, exclude_kernel = 0.
    auto pe = GetPerfEventAttr(PERF_TYPE_TRACEPOINT, 1, /*exclude_kernel=*/false);
    EXPECT_THAT(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // All `pid`s, exclude_kernel = 1.
    pe = GetPerfEventAttr(PERF_TYPE_TRACEPOINT, 1, /*exclude_kernel=*/true);
    EXPECT_THAT(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // Current pid, exclude_kernel = 0.
    pe = GetPerfEventAttr(PERF_TYPE_TRACEPOINT, 1, /*exclude_kernel=*/false);
    EXPECT_THAT(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // Current pid, exclude_kernel = 1.
    pe = GetPerfEventAttr(PERF_TYPE_TRACEPOINT, 1, /*exclude_kernel=*/true);
    EXPECT_THAT(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));
  }));
}

TEST(PerfEventTest, OpenEventsNoPerfmon) {
  auto kEventId = 1;
  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_no_perfmon_t:s0", [&] {
    auto pe = GetPerfEventAttr(PERF_TYPE_TRACEPOINT, kEventId, 0);
    EXPECT_THAT(perf_event_open(pe.get(), -1 /* All tasks */, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));
  }));
}

TEST(PerfEventTest, OpenEventsBreakpoint) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  int watched_variable = 0;
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_all_permissions_t:s0", [&] {
    // All `pid`s, exclude_kernel = 0.
    auto pe = GetPerfEventAttr(PERF_TYPE_BREAKPOINT, 1, /*exclude_kernel=*/false);
    pe->bp_type = HW_BREAKPOINT_W;
    pe->bp_addr = (uint64_t)&watched_variable;
    pe->bp_len = sizeof(int);
    fbl::unique_fd fd(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // All `pid`s, exclude_kernel = 1.
    pe->exclude_kernel = 1;
    fd = fbl::unique_fd(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Current pid, exclude_kernel = 0.
    pe->exclude_kernel = 0;
    fd = fbl::unique_fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Current pid, exclude_kernel = 1.
    pe->exclude_kernel = 1;
    fd = fbl::unique_fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

TEST(PerfEventTest, OpenEventsBreakpointNoKernel) {
  // perf_event_open with type Breakpoint fails when exclude_kernel == 1 and kernel permission is
  // missing.
  auto enforce = ScopedEnforcement::SetEnforcing();
  int watched_variable = 0;
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_no_kernel_t:s0", [&] {
    // All `pid`s, exclude_kernel = 0.
    auto pe = GetPerfEventAttr(PERF_TYPE_BREAKPOINT, 1, /*exclude_kernel=*/false);
    pe->bp_type = HW_BREAKPOINT_W;
    pe->bp_addr = (uint64_t)&watched_variable;
    pe->bp_len = sizeof(int);
    EXPECT_THAT(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // All `pid`s, exclude_kernel = 1.
    pe->exclude_kernel = 1;
    fbl::unique_fd fd(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Current pid, exclude_kernel = 0.
    pe->exclude_kernel = 0;
    EXPECT_THAT(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // Current pid, exclude_kernel = 1.
    pe->exclude_kernel = 1;
    fd = fbl::unique_fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

TEST(PerfEventTest, OpenEventsBreakpointNoCpu) {
  // perf_event_open with type Breakpoint fails when pid == -1 and cpu permission is
  // missing.
  auto enforce = ScopedEnforcement::SetEnforcing();
  int watched_variable = 0;
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_no_cpu_t:s0", [&] {
    // All `pid`s, exclude_kernel = 0.
    auto pe = GetPerfEventAttr(PERF_TYPE_BREAKPOINT, 1, /*exclude_kernel=*/false);
    pe->bp_type = HW_BREAKPOINT_W;
    pe->bp_addr = (uint64_t)&watched_variable;
    pe->bp_len = sizeof(int);
    EXPECT_THAT(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // All `pid`s, exclude_kernel = 1.
    pe->exclude_kernel = 1;
    EXPECT_THAT(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // Current pid, exclude_kernel = 0.
    pe->exclude_kernel = 0;
    fbl::unique_fd fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Current pid, exclude_kernel = 1.
    pe->exclude_kernel = 1;
    fd = fbl::unique_fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

TEST(PerfEventTest, OpenEventsRaw) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  // Define the raw event code (Event 0x24, Umask 0x41, combined)
  auto RAW_L2_RQSTS_EVENT = 0x4124;
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_all_permissions_t:s0", [&] {
    // All `pid`s, exclude_kernel = 0.
    auto pe = GetPerfEventAttr(PERF_TYPE_RAW, RAW_L2_RQSTS_EVENT, /*exclude_kernel=*/false);
    fbl::unique_fd fd(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // All `pid`s, exclude_kernel = 1.
    pe->exclude_kernel = 1;
    fd = fbl::unique_fd(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Current pid, exclude_kernel = 0.
    pe->exclude_kernel = 0;
    fd = fbl::unique_fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Current pid, exclude_kernel = 1.
    pe->exclude_kernel = 1;
    fd = fbl::unique_fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

TEST(PerfEventTest, OpenEventsRawNoKernel) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  // Define the raw event code (Event 0x24, Umask 0x41, combined)
  auto RAW_L2_RQSTS_EVENT = 0x4124;
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_no_kernel_t:s0", [&] {
    // All `pid`s, exclude_kernel = 0.
    auto pe = GetPerfEventAttr(PERF_TYPE_RAW, RAW_L2_RQSTS_EVENT, /*exclude_kernel=*/false);
    EXPECT_THAT(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // All `pid`s, exclude_kernel = 1.
    pe->exclude_kernel = 1;
    fbl::unique_fd fd(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Current pid, exclude_kernel = 0.
    pe->exclude_kernel = 0;
    EXPECT_THAT(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // Current pid, exclude_kernel = 1.
    pe->exclude_kernel = 1;
    fd = fbl::unique_fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

TEST(PerfEventTest, OpenEventsRawNoCpu) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  // Define the raw event code (Event 0x24, Umask 0x41, combined)
  auto RAW_L2_RQSTS_EVENT = 0x4124;
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_no_cpu_t:s0", [&] {
    // All `pid`s, exclude_kernel = 0.
    auto pe = GetPerfEventAttr(PERF_TYPE_RAW, RAW_L2_RQSTS_EVENT, /*exclude_kernel=*/false);
    EXPECT_THAT(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // All `pid`s, exclude_kernel = 1.
    pe->exclude_kernel = 1;
    EXPECT_THAT(perf_event_open(pe.get(), kAllTasksPid, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));

    // Current pid, exclude_kernel = 0.
    pe->exclude_kernel = 0;
    fbl::unique_fd fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());

    // Current pid, exclude_kernel = 1.
    pe->exclude_kernel = 1;
    fd = fbl::unique_fd(perf_event_open(pe.get(), kCurrentTaskPid, 0 /* this CPU */));
    EXPECT_THAT(fd.get(), SyscallSucceeds());
  }));
}

}  // namespace
