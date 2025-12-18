// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/starnix/tests/syscalls/cpp/test_helper.h"

#include <errno.h>
#include <fcntl.h>
#include <lib/fit/defer.h>
#include <sys/resource.h>
#include <unistd.h>

#include <cstring>
#include <thread>

#include <gtest/gtest.h>

namespace {

TEST(TestHelperTest, DetectFailingChildren) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([] { FAIL() << "Expected failure"; });

  EXPECT_FALSE(helper.WaitForChildren());
}

TEST(ScopedTestDirTest, DoesntLeakFileDescriptors) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([] {
    struct rlimit limit;
    ASSERT_EQ(getrlimit(RLIMIT_NOFILE, &limit), 0) << "getrlimit: " << std::strerror(errno);
    auto cleanup = fit::defer([limit]() {
      EXPECT_EQ(setrlimit(RLIMIT_NOFILE, &limit), 0) << "setrlimit: " << std::strerror(errno);
    });

    struct rlimit new_limit = {.rlim_cur = 100, .rlim_max = limit.rlim_max};
    ASSERT_EQ(setrlimit(RLIMIT_NOFILE, &new_limit), 0) << "setrlimit: " << std::strerror(errno);

    for (size_t i = 0; i <= new_limit.rlim_cur; i++) {
      test_helper::ScopedTempDir scoped_dir;
      fbl::unique_fd mem_fd(test_helper::MemFdCreate("try_create", O_WRONLY));
      EXPECT_TRUE(mem_fd.is_valid()) << "memfd_create: " << std::strerror(errno);
    }
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(EventFdSemTest, WaitFromThread) {
  test_helper::EventFdSem sem(0);
  std::thread t1([&sem]() { SAFE_SYSCALL(sem.Wait()); });

  SAFE_SYSCALL(sem.Notify(1));
  t1.join();
}

TEST(EventFdSemTest, WaitFromFork) {
  test_helper::EventFdSem sem(0);
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&sem]() { SAFE_SYSCALL(sem.Wait()); });

  SAFE_SYSCALL(sem.Notify(1));
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(EventFdSemTest, CheckWaitThreaded) {
  test_helper::EventFdSem sem(0);
  std::atomic<bool> flag = false;

  std::thread t1([&sem, &flag]() {
    SAFE_SYSCALL(sem.Wait());
    flag = true;
  });

  EXPECT_EQ(flag, false);
  SAFE_SYSCALL(sem.Notify(1));
  t1.join();
  EXPECT_EQ(flag, true);
}

TEST(EventFdSemTest, NotifyFromThread) {
  test_helper::EventFdSem sem(0);
  std::thread t1([&sem]() { SAFE_SYSCALL(sem.Notify(1)); });

  t1.join();
  SAFE_SYSCALL(sem.Wait());
}

TEST(EventFdSemTest, NotifyFromFork) {
  test_helper::EventFdSem sem(0);
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&sem]() { SAFE_SYSCALL(sem.Notify(1)); });

  EXPECT_TRUE(helper.WaitForChildren());
  SAFE_SYSCALL(sem.Wait());
}

TEST(TestHelperTest, WaitForChildrenWithResults) {
  test_helper::ForkHelper helper;

  pid_t failing_child = helper.RunInForkedProcess([] { exit(1); });
  pid_t succeeding_child = helper.RunInForkedProcess([] { exit(0); });

  helper.ExpectExitValue(0);
  auto results = helper.WaitForChildrenWithResults();

  ASSERT_EQ(results.size(), 2u);

  bool found_failing = false;
  bool found_succeeding = false;
  for (const auto& result : results) {
    if (result.subprocess_id == failing_child) {
      EXPECT_FALSE(result.determined_result);
      EXPECT_EQ(result.subprocess_exit_status, 1);
      found_failing = true;
    } else if (result.subprocess_id == succeeding_child) {
      EXPECT_TRUE(result.determined_result);
      EXPECT_EQ(result.subprocess_exit_status, 0);
      found_succeeding = true;
    }
  }
  EXPECT_TRUE(found_failing);
  EXPECT_TRUE(found_succeeding);
}

TEST(TestHelperTest, WaitForChild) {
  test_helper::ForkHelper helper;

  pid_t failing_child = helper.RunInForkedProcess([] { exit(1); });
  pid_t succeeding_child = helper.RunInForkedProcess([] { exit(0); });

  helper.ExpectExitValue(0);
  auto failing_result = helper.WaitForChild(failing_child);
  EXPECT_FALSE(failing_result.determined_result);
  EXPECT_EQ(failing_result.subprocess_exit_status, 1);

  auto succeeding_result = helper.WaitForChild(succeeding_child);
  EXPECT_TRUE(succeeding_result.determined_result);
  EXPECT_EQ(succeeding_result.subprocess_exit_status, 0);
}

TEST(TestHelperTest, WaitForChildMultipleTimes) {
  test_helper::ForkHelper helper;

  pid_t child1 = helper.RunInForkedProcess([] { exit(1); });
  pid_t child2 = helper.RunInForkedProcess([] { exit(0); });
  pid_t child3 = helper.RunInForkedProcess([] { exit(2); });

  helper.ExpectExitValue(0);

  test_helper::ForkResult result1 = helper.WaitForChild(child1);
  EXPECT_FALSE(result1.determined_result);
  EXPECT_EQ(result1.subprocess_exit_status, 1);

  test_helper::ForkResult result2 = helper.WaitForChild(child2);
  EXPECT_TRUE(result2.determined_result);
  EXPECT_EQ(result2.subprocess_exit_status, 0);

  test_helper::ForkResult result3 = helper.WaitForChild(child3);
  EXPECT_FALSE(result3.determined_result);
  EXPECT_EQ(result3.subprocess_exit_status, 2);
}

TEST(TestHelperTest, WaitForChildThenWaitForChildren) {
  test_helper::ForkHelper helper;

  helper.RunInForkedProcess([] { exit(0); });
  pid_t second_child = helper.RunInForkedProcess([] { exit(20); });
  helper.RunInForkedProcess([] { exit(0); });

  helper.ExpectExitValue(0);

  // Wait for the second child specifically.
  test_helper::ForkResult second_result = helper.WaitForChild(second_child);
  EXPECT_FALSE(second_result.determined_result);
  EXPECT_EQ(second_result.subprocess_exit_status, 20);
  EXPECT_EQ(second_result.subprocess_id, second_child);

  // Wait for the remaining children. The children not explicitly waited for will be reaped here.
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(TestHelperTest, MultipleChildrenPassing) {
  test_helper::ForkHelper helper;

  for (int i = 0; i < 5; i++) {
    helper.RunInForkedProcess([]() { exit(0); });
  }

  helper.ExpectExitValue(0);
  auto results = helper.WaitForChildrenWithResults();
  EXPECT_EQ(results.size(), 5u);
  for (const auto& result : results) {
    EXPECT_TRUE(result.determined_result);
  }
}

TEST(TestHelperTest, MultipleChildrenFailing) {
  test_helper::ForkHelper helper;
  helper.ExpectExitValue(0);

  for (int i = 0; i < 5; i++) {
    helper.RunInForkedProcess([i]() { exit(i + 1); });
  }

  auto results = helper.WaitForChildrenWithResults();
  EXPECT_EQ(results.size(), 5u);
  for (const auto& result : results) {
    EXPECT_FALSE(result.determined_result);
  }
}

}  // namespace
