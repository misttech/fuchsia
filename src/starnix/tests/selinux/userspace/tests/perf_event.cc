// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/syscall.h>
#include <unistd.h>

#include <cstdint>
#include <memory>

#include <gtest/gtest.h>
#include <linux/hw_breakpoint.h> /* Definition of HW_* constants */
#include <linux/perf_event.h>    /* Definition of PERF_* constants */

#include "gmock/gmock.h"
#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

extern std::string DoPrePolicyLoadWork() { return "perf_event.pp"; }

namespace {

int perf_event_open(struct perf_event_attr* attr, pid_t pid, int cpu) {
  return (int)syscall(__NR_perf_event_open, attr, pid, cpu, /*group_fd=*/-1, /*flags=*/0);
}

class PerfEventTest : public ::testing::Test {};

std::unique_ptr<struct perf_event_attr> GetPerfEventAttr(uint32_t type, uint64_t config) {
  auto pe = std::make_unique<struct perf_event_attr>();

  // Set parameters for the event we want to measure:
  pe->type = type;
  pe->size = sizeof(struct perf_event_attr);
  pe->config = config;
  // Enable count for events that happen in kernel space.
  pe->exclude_kernel = 0;

  return pe;
}

TEST(PerfEventTest, PerfEventOpenCpuEventsOnCurrentTask) {
  auto enforce = ScopedEnforcement::SetEnforcing();
  auto cpu_event_types = {PERF_TYPE_HARDWARE, PERF_TYPE_HW_CACHE, PERF_TYPE_SOFTWARE};

  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_all_permissions_t:s0", [&] {
    for (const perf_type_id& type : cpu_event_types) {
      auto pe = GetPerfEventAttr(type, PERF_COUNT_HW_CPU_CYCLES);
      EXPECT_THAT(perf_event_open(pe.get(), 0 /* current task */, 0 /* this CPU */),
                  SyscallSucceeds());
    }
  }));
}

TEST(PerfEventTest, PerfEventOpenKernelEventsOnCurrentTask) {
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_event_all_permissions_t:s0", [&] {
    auto kEventId = 215;
    auto enforce = ScopedEnforcement::SetEnforcing();
    auto pe = GetPerfEventAttr(PERF_TYPE_TRACEPOINT, kEventId);
    EXPECT_THAT(perf_event_open(pe.get(), 0 /* current task */, 0 /* this CPU */),
                SyscallSucceeds());
  }));
}

TEST(PerfEventTest, PerfEventOpenAllTasks) {
  // The kernel SID as defined in base_policy.conf.
  auto kernel_sid = "system_u:unconfined_r:unconfined_t:s0";
  auto kEventId = 215;
  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs(kernel_sid, [&] {
    auto pe = GetPerfEventAttr(PERF_TYPE_TRACEPOINT, kEventId);
    EXPECT_THAT(perf_event_open(pe.get(), -1 /* All tasks */, 0 /* this CPU */), SyscallSucceeds());
  }));
}

TEST(PerfEventTest, PerfEventOpenAllTasksFailsOnUnauthorisedCurrentTask) {
  auto kEventId = 215;
  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:test_perf_not_admin_t:s0", [&] {
    auto pe = GetPerfEventAttr(PERF_TYPE_TRACEPOINT, kEventId);
    EXPECT_THAT(perf_event_open(pe.get(), -1 /* All tasks */, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));
  }));
}

// perf_event_open fails for CPU events due to missing permissions.
class PerfEventFailingCpuEvents : public PerfEventTest,
                                  public testing::WithParamInterface<std::string_view> {};

TEST_P(PerfEventFailingCpuEvents, PerfEventOpenFailsOnMissingPermissions) {
  const auto label = PerfEventFailingCpuEvents::GetParam();

  auto enforce = ScopedEnforcement::SetEnforcing();
  auto hardware_event = GetPerfEventAttr(PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES);

  // Missing `open` permission.
  ASSERT_TRUE(RunSubprocessAs(label, [&] {
    EXPECT_THAT(perf_event_open(hardware_event.get(), 0 /* current task */, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));
  }));
}

const auto kPerfEventFailingCpuEventsContexts = ::testing::Values(
    // Missing `open` permission.
    "test_u:test_r:test_perf_event_no_open_t:s0",
    // Missing `cpu` permission.
    "test_u:test_r:test_perf_event_no_cpu_t:s0");
INSTANTIATE_TEST_SUITE_P(PerfEventTest, PerfEventFailingCpuEvents,
                         kPerfEventFailingCpuEventsContexts);

// perf_event_open fails for Kernel events due to missing permissions.
class PerfEventFailingKernelEvents : public PerfEventTest,
                                     public testing::WithParamInterface<std::string_view> {};

TEST_P(PerfEventFailingKernelEvents, PerfEventOpenFailsOnMissingPermissions) {
  const auto label = PerfEventFailingKernelEvents::GetParam();

  auto enforce = ScopedEnforcement::SetEnforcing();
  auto kEventId = 215;

  auto tracepoint_event = GetPerfEventAttr(PERF_TYPE_TRACEPOINT, kEventId);

  // Missing `open` permission.
  ASSERT_TRUE(RunSubprocessAs(label, [&] {
    EXPECT_THAT(perf_event_open(tracepoint_event.get(), 0 /* current task */, 0 /* this CPU */),
                SyscallFailsWithErrno(EACCES));
  }));
}

const auto kPerfEventFailingKernelEventsContexts = ::testing::Values(
    // Missing `open` permission.
    "test_u:test_r:test_perf_event_no_open_t:s0",
    // Missing `kernel` permission.
    "test_u:test_r:test_perf_event_no_kernel_t:s0",
    // Missing `tracepoint` permission.
    "test_u:test_r:test_perf_event_no_tracepoint_t:s0");
INSTANTIATE_TEST_SUITE_P(PerfEventTest, PerfEventFailingKernelEvents,
                         kPerfEventFailingKernelEventsContexts);

}  // namespace
