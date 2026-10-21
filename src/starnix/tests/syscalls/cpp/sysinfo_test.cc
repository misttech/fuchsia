// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <sched.h>
#include <sys/syscall.h>
#include <sys/sysinfo.h>
#include <sys/wait.h>
#include <unistd.h>

#include <thread>

#include <gtest/gtest.h>
#include <linux/sched.h>

#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

TEST(SysinfoTest, BasicSysinfo) {
  struct sysinfo info = {};
  ASSERT_EQ(sysinfo(&info), 0);
  EXPECT_GT(info.mem_unit, 0U);
  if (sizeof(info.totalram) == 8) {
    EXPECT_EQ(info.mem_unit, 1U);
  }
  EXPECT_GT(info.totalram, 0U);
  EXPECT_GT(info.freeram, 0U);
  EXPECT_GE(info.procs, 1U);
}

TEST(SysinfoTest, ProcsTracksTaskLifecycle) {
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP() << "Deterministic sysinfo.procs accounting requires an isolated Starnix kernel";
  }

  struct sysinfo info = {};
  ASSERT_EQ(sysinfo(&info), 0);
  const uint16_t initial_procs = info.procs;
  ASSERT_GE(initial_procs, 1U);

  // A failing clone3 for a new process (null child_tid with CLONE_CHILD_SETTID) must roll back
  // and leave info.procs unchanged.
  struct clone_args ca = {};
  ca.flags = CLONE_CHILD_SETTID;
  ASSERT_EQ(syscall(__NR_clone3, &ca, sizeof(ca)), -1);
  EXPECT_EQ(errno, EFAULT);
  ASSERT_EQ(sysinfo(&info), 0);
  EXPECT_EQ(info.procs, initial_procs);
  while (waitpid(-1, nullptr, __WALL | WNOHANG) > 0) {
  }

  // Spawning a thread in the same thread group must increment info.procs while the thread is live,
  // and decrement it once the thread exits.
  pid_t worker_tid = 0;
  test_helper::Rendezvous thread_ready = test_helper::MakeRendezvous();
  test_helper::Rendezvous thread_exit = test_helper::MakeRendezvous();
  std::thread worker([&worker_tid, ready = std::move(thread_ready.poker),
                      exit = std::move(thread_exit.holder)]() mutable {
    worker_tid = static_cast<pid_t>(syscall(SYS_gettid));
    ready.poke();
    exit.hold();
  });
  thread_ready.holder.hold();

  ASSERT_EQ(sysinfo(&info), 0);
  EXPECT_EQ(info.procs, initial_procs + 1);

  thread_exit.poker.poke();
  worker.join();
  // pthread_join wakes on CLONE_CHILD_CLEARTID before the kernel finishes removing the task from
  // the PID table in CurrentTask::exit; wait until the TID is no longer in the PID table.
  while (syscall(SYS_tgkill, getpid(), worker_tid, 0) == 0) {
    sched_yield();
  }
  EXPECT_EQ(errno, ESRCH);

  ASSERT_EQ(sysinfo(&info), 0);
  EXPECT_EQ(info.procs, initial_procs);

  // Forking a child process must increment info.procs while the child is live.
  test_helper::Rendezvous child_ready = test_helper::MakeRendezvous();
  test_helper::Rendezvous child_exit = test_helper::MakeRendezvous();
  pid_t child_pid = fork();
  ASSERT_GE(child_pid, 0);
  if (child_pid == 0) {
    child_ready.poker.poke();
    child_exit.holder.hold();
    _exit(0);
  }

  child_ready.holder.hold();
  ASSERT_EQ(sysinfo(&info), 0);
  EXPECT_EQ(info.procs, initial_procs + 1);

  // Allow the child to exit and wait for it to become an unreaped zombie via WNOWAIT.
  child_exit.poker.poke();
  siginfo_t siginfo = {};
  ASSERT_EQ(HANDLE_EINTR(waitid(P_PID, child_pid, &siginfo, WEXITED | WNOWAIT)), 0);
  EXPECT_EQ(siginfo.si_pid, child_pid);

  // Once the child transitions to a zombie, info.procs must decrement back to initial_procs.
  ASSERT_EQ(sysinfo(&info), 0);
  EXPECT_EQ(info.procs, initial_procs);

  // Reap the zombie and verify info.procs remains at initial_procs.
  int status = 0;
  ASSERT_EQ(HANDLE_EINTR(waitpid(child_pid, &status, 0)), child_pid);
  EXPECT_TRUE(WIFEXITED(status));
  EXPECT_EQ(WEXITSTATUS(status), 0);

  ASSERT_EQ(sysinfo(&info), 0);
  EXPECT_EQ(info.procs, initial_procs);
}

}  // namespace
