// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <lib/fit/defer.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <atomic>
#include <barrier>
#include <chrono>
#include <ctime>
#include <latch>
#include <new>
#include <optional>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <linux/capability.h>
#include <linux/futex.h>

#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/starnix/tests/syscalls/cpp/capabilities_helper.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

#ifndef MAP_FIXED_NOREPLACE
#define MAP_FIXED_NOREPLACE 0x100000
#endif

namespace {

struct robust_list_entry {
  struct robust_list *next;
  int futex;
};

// Tests that robust_lists set futex FUTEX_OWNER_DIED bit if the thread that locked a futex
// dies without unlocking it.
TEST(RobustFutexTest, FutexStateCheck) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    robust_list_entry entry = {.next = nullptr, .futex = 0};
    robust_list_head head = {.list = {.next = nullptr},
                             .futex_offset = offsetof(robust_list_entry, futex),
                             .list_op_pending = nullptr};

    std::thread t([&entry, &head]() {
      head.list.next = reinterpret_cast<struct robust_list *>(&entry);
      SAFE_SYSCALL(syscall(SYS_set_robust_list, &head, sizeof(robust_list_head)));
      entry.futex = static_cast<int>(syscall(SYS_gettid));
      entry.next = reinterpret_cast<struct robust_list *>(&head);
      // Thread dies without releasing futex, so futex's FUTEX_OWNER_DIED bit is set.
    });
    t.join();
    EXPECT_EQ(FUTEX_OWNER_DIED, entry.futex & FUTEX_OWNER_DIED);
  });
}

// Tests that entries with a tid different than the current tid are ignored.
TEST(RobustFutexTest, OtherTidsAreIgnored) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    constexpr size_t kNumEntries = 3;
    robust_list_entry entries[kNumEntries] = {};
    robust_list_head head = {.list = {.next = nullptr},
                             .futex_offset = offsetof(robust_list_entry, futex),
                             .list_op_pending = nullptr};

    head.list.next = reinterpret_cast<struct robust_list *>(&entries[0]);
    for (size_t i = 0; i < kNumEntries - 1; i++) {
      entries[i].next = reinterpret_cast<struct robust_list *>(&entries[i + 1]);
    }
    entries[kNumEntries - 1].next = reinterpret_cast<struct robust_list *>(&head);

    int parent_tid = static_cast<int>(syscall(SYS_gettid));

    std::thread t([&entries, &head, parent_tid]() {
      SAFE_SYSCALL(syscall(SYS_set_robust_list, &head, sizeof(robust_list_head)));
      int tid = static_cast<int>(syscall(SYS_gettid));
      entries[0].futex = tid;
      entries[1].futex = parent_tid;
      entries[2].futex = tid;
    });
    t.join();
    // We expect the first and list entries to be correctly modified.
    // The second entry (wrong tid) should remain unchanged.
    EXPECT_EQ(FUTEX_OWNER_DIED, entries[0].futex & FUTEX_OWNER_DIED);
    EXPECT_EQ(FUTEX_OWNER_DIED, entries[2].futex & FUTEX_OWNER_DIED);
    EXPECT_EQ(parent_tid, entries[1].futex);
  });
}

// Tests that an entry with next = NULL doesn't cause issues.
TEST(RobustFutexTest, NullEntryStopsProcessing) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    robust_list_entry entry = {.next = nullptr, .futex = 0};
    robust_list_head head = {.list = {.next = nullptr},
                             .futex_offset = offsetof(robust_list_entry, futex),
                             .list_op_pending = nullptr};

    std::thread t([&entry, &head]() {
      head.list.next = reinterpret_cast<struct robust_list *>(&entry);
      SAFE_SYSCALL(syscall(SYS_set_robust_list, &head, sizeof(robust_list_head)));
      entry.futex = static_cast<int>(syscall(SYS_gettid));
      entry.next = nullptr;
    });
    t.join();
    // We expect the first entry to be correctly modified.
    EXPECT_EQ(FUTEX_OWNER_DIED, entry.futex & FUTEX_OWNER_DIED);
  });
}

// Test that exceeding the maximum number of robust futexes would lead to a
// futex not being processed.
TEST(RobustFutexTest, RobustListLimitIsEnforced) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    constexpr size_t kNumEntries = ROBUST_LIST_LIMIT + 1;
    robust_list_entry entries[kNumEntries] = {};
    robust_list_head head = {.list = {.next = nullptr},
                             .futex_offset = offsetof(robust_list_entry, futex),
                             .list_op_pending = nullptr};

    head.list.next = reinterpret_cast<struct robust_list *>(&entries[0]);
    for (size_t i = 0; i < kNumEntries - 1; i++) {
      entries[i].next = reinterpret_cast<struct robust_list *>(&entries[i + 1].next);
    }
    entries[kNumEntries - 1].next = reinterpret_cast<struct robust_list *>(&head);

    std::thread t([&entries, &head]() {
      int tid = static_cast<int>(syscall(SYS_gettid));
      for (size_t i = 0; i < kNumEntries; i++) {
        entries[i].futex = tid;
      }

      SAFE_SYSCALL(syscall(SYS_set_robust_list, &head, sizeof(robust_list_head)));
    });

    t.join();

    for (size_t i = 0; i < kNumEntries - 1; i++) {
      EXPECT_EQ(FUTEX_OWNER_DIED, entries[i].futex & FUTEX_OWNER_DIED);
    }
    // The last entry was not modified.
    EXPECT_EQ(0, entries[kNumEntries - 1].futex & FUTEX_OWNER_DIED);
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

struct unaligned_robust_list_entry {
  struct robust_list *next;
  char unused;
  int futex;
} __attribute__((packed, aligned(4)));
static_assert(offsetof(unaligned_robust_list_entry, futex) % 4 != 0,
              "futex lock offset must be unaligned");

TEST(RobustFutexTest, RobustListEnforcesAlignment) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    unaligned_robust_list_entry entry = {.next = nullptr, .unused = 0, .futex = 0};
    robust_list_head head = {.list = {.next = nullptr},
                             .futex_offset = offsetof(unaligned_robust_list_entry, futex),
                             .list_op_pending = nullptr};

    std::thread t([&entry, &head]() {
      head.list.next = reinterpret_cast<struct robust_list *>(&entry);
      SAFE_SYSCALL(syscall(SYS_set_robust_list, &head, sizeof(robust_list_head)));
      entry.futex = static_cast<int>(syscall(SYS_gettid));
      entry.next = reinterpret_cast<struct robust_list *>(&head);
    });
    t.join();
    // The entry was not modified.
    EXPECT_EQ(0, entry.futex & FUTEX_OWNER_DIED);
  });
}

TEST(RobustFutexTest, DoesNotModifyReadOnlyMapping) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    const size_t page_size = sysconf(_SC_PAGESIZE);
    void *addr = mmap(nullptr, sizeof(page_size), PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(addr, MAP_FAILED);

    robust_list_head head = {.list = {.next = nullptr},
                             .futex_offset = offsetof(robust_list_entry, futex),
                             .list_op_pending = nullptr};

    robust_list_entry *entry = new (addr) robust_list_entry;

    std::thread t([entry, &head, addr, page_size]() {
      head.list.next = reinterpret_cast<struct robust_list *>(entry);
      SAFE_SYSCALL(syscall(SYS_set_robust_list, &head, sizeof(robust_list_head)));
      entry->futex = static_cast<int>(syscall(SYS_gettid));
      entry->next = reinterpret_cast<struct robust_list *>(&head);

      SAFE_SYSCALL(mprotect(addr, page_size, PROT_READ));
    });
    t.join();
    // Memory allocating futex is not writable, so it should not be modified by the kernel.
    EXPECT_EQ(0, entry->futex & FUTEX_OWNER_DIED);
    entry->~robust_list_entry();
    SAFE_SYSCALL(munmap(addr, page_size));
  });

  EXPECT_TRUE(helper.WaitForChildren());
}

// Tests that issuing a cyclic robust list doesn't hang the starnix kernel.
TEST(RobustFutexTest, CyclicRobustListDoesntHang) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    robust_list_entry entry1 = {.next = nullptr, .futex = 0};
    robust_list_entry entry2 = {.next = nullptr, .futex = 0};
    robust_list_head head = {.list = {.next = nullptr},
                             .futex_offset = offsetof(robust_list_entry, futex),
                             .list_op_pending = nullptr};

    std::thread t([&entry1, &entry2, &head]() {
      entry1.next = reinterpret_cast<struct robust_list *>(&entry2);
      entry2.next = reinterpret_cast<struct robust_list *>(&entry1);

      head.list.next = reinterpret_cast<struct robust_list *>(&entry1);
      SAFE_SYSCALL(syscall(SYS_set_robust_list, &head, sizeof(robust_list_head)));
    });
    t.join();
    // Our robust list has a cycle. We should be able to stop correctly.
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

// Tests that robust_lists set futex FUTEX_OWNER_DIED bit if the thread that locked a futex
// executes an exec() without unlocking it.
TEST(RobustFutexTest, FutexStateAfterExecCheck) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    // Allocate the futex and the robust list in shared memory.
    void *shared = mmap(nullptr, sizeof(robust_list_entry) + sizeof(robust_list_head),
                        PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    EXPECT_NE(MAP_FAILED, shared) << "Error " << errno;

    robust_list_head *head = reinterpret_cast<robust_list_head *>(shared);
    robust_list_entry *entry = reinterpret_cast<robust_list_entry *>(
        reinterpret_cast<intptr_t>(shared) + sizeof(robust_list_head));

    *entry = {.next = reinterpret_cast<struct robust_list *>(head), .futex = 0};
    *head = {.list = {.next = reinterpret_cast<struct robust_list *>(entry)},
             .futex_offset = offsetof(robust_list_entry, futex),
             .list_op_pending = nullptr};

    // Create a pipe that the child can use to notify parent process when it is running.
    int pipefd[2];
    pipe(pipefd);

    // Create a file we can lock.  After it notifies us that it is running via
    // the pipe, the child will wait to terminate until we unlock the file.
    test_helper::ScopedTempFD terminate_child_fd;
    struct flock fl = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
    SAFE_SYSCALL(fcntl(terminate_child_fd.fd(), F_SETLK, &fl));

    test_helper::ForkHelper helper;

    helper.RunInForkedProcess([&entry, &head, &terminate_child_fd, &pipefd] {
      // Redirect stdout to one end of the pipe
      EXPECT_NE(-1, dup2(pipefd[1], STDOUT_FILENO));
      SAFE_SYSCALL(syscall(SYS_set_robust_list, head, sizeof(robust_list_head)));
      entry->futex = static_cast<int>(syscall(SYS_gettid));

      std::string test_binary = test_helper::GetTestResourcePath("syscall_test_exec_child");
      char *const argv[] = {const_cast<char *>(test_binary.c_str()),
                            const_cast<char *>(terminate_child_fd.name().c_str()), nullptr};

      // execv happens without releasing futex, so futex's FUTEX_OWNER_DIED bit is set.
      execv(test_binary.c_str(), argv);
    });

    char buf[5];
    // Wait until the child process has performed the exec
    EXPECT_NE(-1, read(pipefd[0], reinterpret_cast<void *>(buf), 5));

    EXPECT_EQ(FUTEX_OWNER_DIED, entry->futex & FUTEX_OWNER_DIED);

    // Unlock the file, allowing the child process to continue (and exit).
    struct flock fl2 = {.l_type = F_UNLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 0};
    SAFE_SYSCALL(fcntl(terminate_child_fd.fd(), F_SETLK, &fl2));
    EXPECT_EQ(true, helper.WaitForChildren());
    munmap(shared, sizeof(robust_list_entry) + sizeof(robust_list_head));
  });
}

TEST(FutexTest, FutexAddressHasToBeAligned) {
  uint32_t some_addresses[] = {0, 0};
  uintptr_t addr = reinterpret_cast<uintptr_t>(&some_addresses[0]);

  auto futex_basic = [](uintptr_t addr, uint32_t op, uint32_t val) {
    return syscall(SYS_futex, addr, op, val, nullptr, nullptr, 0);
  };

  auto futex_requeue = [](uintptr_t addr, uint32_t val, uint32_t val2, uintptr_t addr2) {
    return syscall(SYS_futex, addr, FUTEX_REQUEUE, val, val2, addr2, 0);
  };

  for (size_t i = 1; i <= 3; i++) {
    EXPECT_EQ(-1, futex_basic(addr + i, FUTEX_WAIT, 0));
    EXPECT_EQ(errno, EINVAL);
    EXPECT_EQ(-1, futex_basic(addr + i, FUTEX_WAIT_PRIVATE, 0));
    EXPECT_EQ(errno, EINVAL);
    EXPECT_EQ(-1, futex_basic(addr + i, FUTEX_WAKE, 0));
    EXPECT_EQ(errno, EINVAL);
    EXPECT_EQ(-1, futex_basic(addr + i, FUTEX_WAKE_PRIVATE, 0));
    EXPECT_EQ(errno, EINVAL);
    EXPECT_EQ(-1, futex_requeue(addr, 0, 0, addr + 4 + i));
    EXPECT_EQ(errno, EINVAL);
  }
}

TEST(FutexTest, FutexAddressOutOfRange) {
  uintptr_t addr = static_cast<uintptr_t>(-4);  // not in userspace

  auto futex_basic = [](uintptr_t addr, uint32_t op, uint32_t val) {
    return syscall(SYS_futex, addr, op, val, nullptr, nullptr, 0);
  };

  EXPECT_EQ(-1, futex_basic(addr, FUTEX_WAIT, 0));
  EXPECT_EQ(errno, EFAULT);
  EXPECT_EQ(-1, futex_basic(addr, FUTEX_WAIT_PRIVATE, 0));
  EXPECT_EQ(errno, EFAULT);
}

TEST(FutexTest, FutexWaitOnRemappedMemory) {
  // This test is inherently racy, and could be flaky:
  // We are trying to race between the FUTEX_WAIT and mmap+FUTEX_WAKE
  // operations. We want to make sure that if we remap the futex
  // page, we don't get threads stuck.
  //
  // See b/298664027 for details.
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    constexpr size_t kNumWaiters = 16;
    constexpr uint32_t kFutexConstant = 0xbeef;
    const size_t page_size = sysconf(_SC_PAGESIZE);
    void *addr =
        mmap(nullptr, page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(addr, MAP_FAILED);
    auto futex_basic = [](std::atomic<uint32_t> *addr, uint32_t op, uint32_t val) {
      return syscall(SYS_futex, reinterpret_cast<uint32_t *>(addr), op, val, nullptr, nullptr, 0);
    };

    std::atomic<uint32_t> *futex = new (addr) std::atomic<uint32_t>(kFutexConstant);

    std::latch wait_for_all_threads(kNumWaiters + 1);

    std::vector<std::thread> waiters;
    for (size_t i = 0; i < kNumWaiters; i++) {
      waiters.emplace_back([&wait_for_all_threads, futex, &futex_basic]() {
        wait_for_all_threads.arrive_and_wait();

        while (futex->load() == kFutexConstant) {
          long res = futex_basic(futex, FUTEX_WAIT_PRIVATE, kFutexConstant);
          EXPECT_TRUE(res == 0 || (res == -1 && errno == EAGAIN));
        }
        EXPECT_EQ(futex->load(), 0u);
      });
    }

    wait_for_all_threads.arrive_and_wait();

    void *new_addr = mmap(addr, page_size, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    ASSERT_NE(new_addr, MAP_FAILED);
    ASSERT_EQ(new_addr, addr);
    futex->store(0);
    long res = futex_basic(futex, FUTEX_WAKE_PRIVATE, INT_MAX);
    EXPECT_TRUE(res >= 0);

    for (auto &waiter : waiters) {
      waiter.join();
    }
    SAFE_SYSCALL(munmap(addr, page_size));
  });
  EXPECT_EQ(true, helper.WaitForChildren());
}

// Test that FUTEX_WAIT can be restarted after being interrupted by a signal.
TEST(FutexTest, WaitRestartableOnSignal) {
  // The child process will do a FUTEX_WAIT with a timeout. The parent will send SIGSTOP + SIGCONT
  // during the timeout.
  test_helper::ForkHelper helper;
  pid_t child_pid = helper.RunInForkedProcess([] {
    uint32_t word = 0;
    struct timespec timeout = {.tv_sec = 1};
    // Should fail with ETIMEDOUT and *not* EINTR.
    ASSERT_THAT(syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, &timeout),
                SyscallFailsWithErrno(ETIMEDOUT));
  });

  // Wait for child to go to sleep.
  std::cerr << "wait for block" << std::endl;
  ASSERT_TRUE(test_helper::WaitUntilBlocked(child_pid, true));
  std::cerr << "waited for block" << std::endl;
  usleep(100000);

  ASSERT_THAT(kill(child_pid, SIGSTOP), SyscallSucceeds());
  ASSERT_THAT(kill(child_pid, SIGCONT), SyscallSucceeds());
  EXPECT_TRUE(helper.WaitForChildren());
}

void NoopSignalHandler(int) {}

std::atomic<int> g_signal_count{0};

void CountingSignalHandler(int) { g_signal_count.fetch_add(1, std::memory_order_relaxed); }

std::atomic<std::atomic<uint32_t> *> g_signal_futex_word{nullptr};

void ValueModifyingSignalHandler(int) {
  std::atomic<uint32_t> *futex_word = g_signal_futex_word.load(std::memory_order_relaxed);
  if (futex_word != nullptr) {
    futex_word->store(1, std::memory_order_relaxed);
  }
  g_signal_count.fetch_add(1, std::memory_order_relaxed);
}

// The flavor of FUTEX_WAIT to exercise. Each of them is backed by a different kind of waiter
// inside the kernel, so each needs its own coverage.
struct WakeRaceTestCase {
  const char *name;
  int wait_op;
  int wake_op;
  uint32_t bitset;
  // Whether the wait takes an absolute CLOCK_REALTIME deadline rather than a relative one.
  bool absolute_realtime_deadline;
};

class FutexWakeRaceTest : public ::testing::TestWithParam<WakeRaceTestCase> {};

// A FUTEX_WAKE that reports having woken a waiter must really wake it up, even when the waiter is
// interrupted by a signal at the same time.
//
// A kernel that resolves this race in favor of the interruption loses the wake: the waiter goes
// back to sleep while the waker believes it has been woken and stops retrying. This is what made
// the LTP pidfd_send_signal01 test flaky, see https://fxbug.dev/559170416.
TEST_P(FutexWakeRaceTest, ReportedWakeIsNotLost) {
  const WakeRaceTestCase &params = GetParam();

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&params] {
    struct sigaction action = {};
    action.sa_handler = NoopSignalHandler;
    // Deliberately no SA_RESTART: the interrupted wait is restarted by userspace, just like the
    // checkpoints of the LTP test suite do.
    sigemptyset(&action.sa_mask);
    SAFE_SYSCALL(sigaction(SIGUSR1, &action, nullptr));

    const pid_t pid = getpid();
    // The futex word never changes, so a waiter can only be released by a FUTEX_WAKE.
    std::atomic<uint32_t> futex_word(0);

    // How long a waiter sleeps before giving up. A waiter that hits this deadline has lost its
    // wake, and the timeout also bounds how long a failing iteration takes to join.
    constexpr int kWaitTimeoutSeconds = 5;
    // Polling budget, in milliseconds, for a wake to be reported and then observed.
    constexpr int kMaxRetries = 5000;
    constexpr int kIterations = 200;

    for (int i = 0; i < kIterations; ++i) {
      std::atomic<pid_t> waiter_tid(0);
      std::atomic<bool> woken(false);

      std::thread waiter([&params, &futex_word, &waiter_tid, &woken]() {
        waiter_tid.store(static_cast<pid_t>(syscall(SYS_gettid)));
        for (;;) {
          struct timespec timeout = {};
          if (params.absolute_realtime_deadline) {
            clock_gettime(CLOCK_REALTIME, &timeout);
            timeout.tv_sec += kWaitTimeoutSeconds;
          } else {
            timeout.tv_sec = kWaitTimeoutSeconds;
          }
          long result =
              syscall(SYS_futex, &futex_word, params.wait_op, 0, &timeout, nullptr, params.bitset);
          if (result == 0) {
            woken.store(true);
            return;
          }
          // Anything but an interruption (in particular a timeout) means the wake was lost.
          if (errno != EINTR) {
            return;
          }
        }
      });

      while (waiter_tid.load() == 0) {
        sched_yield();
      }

      // The waiter always returns, at the latest when its deadline expires, so the thread can be
      // joined on every path out of this lambda. Reporting failures by returning rather than with
      // ASSERT_* is what keeps a still-joinable thread from tripping std::terminate.
      const bool keep_going = [&]() {
        if (!test_helper::WaitForTaskState(pid, waiter_tid.load(),
                                           [](std::string_view state) { return state == "S"; })) {
          ADD_FAILURE() << "waiter never blocked at iteration " << i;
          return false;
        }

        // Interrupt the waiter and immediately try to wake it, so that the wake races with the
        // interruption.
        SAFE_SYSCALL(syscall(SYS_tgkill, pid, waiter_tid.load(), SIGUSR1));

        long woken_count = SAFE_SYSCALL(
            syscall(SYS_futex, &futex_word, params.wake_op, 1, nullptr, nullptr, params.bitset));
        // The waiter may have been dequeued by the interruption before it could be woken. Retry
        // until a waiter is reported as woken, exactly like tst_checkpoint_wake() does.
        for (int retry = 0; woken_count == 0 && retry < kMaxRetries; ++retry) {
          usleep(1000);
          woken_count = SAFE_SYSCALL(
              syscall(SYS_futex, &futex_word, params.wake_op, 1, nullptr, nullptr, params.bitset));
        }
        if (woken_count != 1) {
          ADD_FAILURE() << "no waiter woken at iteration " << i;
          return false;
        }

        // A waiter was reported as woken, so the waiter must return from its wait.
        for (int retry = 0; !woken.load() && retry < kMaxRetries; ++retry) {
          usleep(1000);
        }
        if (!woken.load()) {
          ADD_FAILURE() << "FUTEX_WAKE reported a wake that never happened at iteration " << i;
          return false;
        }
        return true;
      }();

      waiter.join();
      if (!keep_going) {
        break;
      }
    }
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

INSTANTIATE_TEST_SUITE_P(
    FutexTest, FutexWakeRaceTest,
    ::testing::Values(
        WakeRaceTestCase{.name = "Shared",
                         .wait_op = FUTEX_WAIT,
                         .wake_op = FUTEX_WAKE,
                         .bitset = FUTEX_BITSET_MATCH_ANY,
                         .absolute_realtime_deadline = false},
        WakeRaceTestCase{.name = "Private",
                         .wait_op = FUTEX_WAIT_PRIVATE,
                         .wake_op = FUTEX_WAKE_PRIVATE,
                         .bitset = FUTEX_BITSET_MATCH_ANY,
                         .absolute_realtime_deadline = false},
        // A realtime deadline parks the caller on a different kind of waiter inside the kernel.
        WakeRaceTestCase{.name = "PrivateBitsetRealtime",
                         .wait_op = FUTEX_WAIT_BITSET_PRIVATE | FUTEX_CLOCK_REALTIME,
                         .wake_op = FUTEX_WAKE_BITSET_PRIVATE,
                         .bitset = 0x5a5a5a5a,
                         .absolute_realtime_deadline = true}),
    [](const ::testing::TestParamInfo<WakeRaceTestCase> &info) { return info.param.name; });

// FUTEX_UNLOCK_PI hands the mutex to a waiter by writing its tid into the futex word, so a waiter
// that is interrupted at that moment must still be told it owns the lock.
//
// A kernel that answers EINTR here leaves the mutex owned by a thread that believes it failed to
// take it: every other locker then blocks forever, and glibc turns the retry into EDEADLK.
TEST(FutexTest, LockPiHandoffIsNotLostWhenRacingWithSignal) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    struct sigaction action = {};
    action.sa_handler = NoopSignalHandler;
    sigemptyset(&action.sa_mask);
    SAFE_SYSCALL(sigaction(SIGUSR1, &action, nullptr));

    const pid_t pid = getpid();
    const pid_t owner_tid = static_cast<pid_t>(syscall(SYS_gettid));

    constexpr int kIterations = 200;
    for (int i = 0; i < kIterations; ++i) {
      // The main thread owns the mutex to force the waiter down the blocking path.
      std::atomic<uint32_t> futex_word(static_cast<uint32_t>(owner_tid));
      std::atomic<pid_t> waiter_tid(0);
      std::atomic<long> lock_result(-1);
      std::atomic<int> lock_errno(0);

      std::thread waiter([&futex_word, &waiter_tid, &lock_result, &lock_errno]() {
        waiter_tid.store(static_cast<pid_t>(syscall(SYS_gettid)));
        long result = syscall(SYS_futex, &futex_word, FUTEX_LOCK_PI_PRIVATE, 0, nullptr);
        lock_errno.store(result == 0 ? 0 : errno);
        lock_result.store(result);
      });

      while (waiter_tid.load() == 0) {
        sched_yield();
      }

      const bool keep_going = [&]() {
        if (!test_helper::WaitForTaskState(pid, waiter_tid.load(),
                                           [](std::string_view state) { return state == "S"; })) {
          ADD_FAILURE() << "waiter never blocked at iteration " << i;
          return false;
        }

        // Interrupt the waiter and hand it the mutex at the same time.
        SAFE_SYSCALL(syscall(SYS_tgkill, pid, waiter_tid.load(), SIGUSR1));
        SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_UNLOCK_PI_PRIVATE));
        return true;
      }();

      waiter.join();
      if (!keep_going) {
        break;
      }

      // Either the unlock handed the mutex over before the signal dequeued the waiter, or it
      // did not and the waiter reports EINTR. What must never happen is the mutex being handed
      // to a waiter that is told its lock failed.
      const uint32_t word = futex_word.load();
      const bool waiter_owns_mutex =
          (word & FUTEX_TID_MASK) == static_cast<uint32_t>(waiter_tid.load());
      const bool lock_succeeded = lock_result.load() == 0;
      EXPECT_EQ(waiter_owns_mutex, lock_succeeded)
          << "futex word " << word << " disagrees with FUTEX_LOCK_PI returning "
          << lock_result.load() << " (errno " << lock_errno.load() << ") at iteration " << i;
      if (waiter_owns_mutex != lock_succeeded) {
        break;
      }

      // Give the mutex back to the main thread for the next iteration.
      futex_word.store(static_cast<uint32_t>(owner_tid));
    }
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(FutexTest, CanRequeueAllWaiters) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    auto futex_basic = [](std::atomic<uint32_t> *addr, uint32_t op, uint32_t val) {
      return syscall(SYS_futex, addr, op, val, nullptr, nullptr, 0);
    };

    auto futex_requeue_all = [](std::atomic<uint32_t> *addr, std::atomic<uint32_t> *addr2) {
      return syscall(SYS_futex, addr, FUTEX_REQUEUE, 0, INT_MAX, addr2, 0);
    };

    std::atomic<uint32_t> futex_word = 0;
    std::atomic<uint32_t> requeue_futex_word = 0;
    std::atomic<size_t> awakened = 0;

    auto waiter_func = [&futex_word, &awakened, &futex_basic]() {
      long res = SAFE_SYSCALL(futex_basic(&futex_word, FUTEX_WAIT, 0));
      EXPECT_EQ(res, 0);
      awakened++;
    };

    std::vector<std::thread> threads;

    constexpr size_t kNumThreads = 10;
    for (size_t i = 0; i < kNumThreads; i++) {
      threads.push_back(std::thread(waiter_func));
    }

    EXPECT_EQ(awakened, 0u);

    long requeued = 0;
    while (requeued != kNumThreads) {
      requeued += SAFE_SYSCALL(futex_requeue_all(&futex_word, &requeue_futex_word));
      sched_yield();
    }

    EXPECT_EQ(awakened, 0u);

    // We cannot wake anyone in the first futex.
    futex_word = 1;
    EXPECT_EQ(futex_basic(&futex_word, FUTEX_WAKE, INT_MAX), 0);

    // We can wake kNumThreads in the second futex.
    requeue_futex_word = 1;
    while (awakened != kNumThreads) {
      SAFE_SYSCALL(futex_basic(&requeue_futex_word, FUTEX_WAKE, INT_MAX));
    }

    for (auto &thread : threads) {
      thread.join();
    }
  });
}

TEST(FutexTest, FutexFailsWithEFAULTOnNullAddress) {
  EXPECT_THAT(syscall(SYS_futex, nullptr, FUTEX_WAIT, 0, nullptr, 0, 0),
              SyscallFailsWithErrno(EFAULT));
}

TEST(FutexTest, FutexFailsWithEFAULTOnInvalidLowAddress) {
  // Zircon forbids creating mappings with addresses lower than 2MB.
  constexpr uintptr_t kInvalidLowAddress = 0x10000;
  EXPECT_THAT(syscall(SYS_futex, kInvalidLowAddress, FUTEX_WAIT, 0, nullptr, 0, 0),
              SyscallFailsWithErrno(EFAULT));
}

#if defined(__x86_64__)
// From zircon/kernel/arch/x86/include/arch/kernel_aspace.h
constexpr uintptr_t kLowestNormalModeAddress = ((1ULL << 46));
#elif defined(__aarch64__)
// From zircon/kernel/arch/arm64/include/arch/kernel_aspace.h
constexpr uintptr_t kLowestNormalModeAddress = ((1ULL << 47));
#elif defined(__arm__)
constexpr uintptr_t kLowestNormalModeAddress = 0xffff0000;
#elif defined(__riscv)
// From zircon/kernel/arch/riscv64/include/arch/kernel_aspace.h
// Currently we only support the RV39 address space model.
constexpr uintptr_t kLowestNormalModeAddress = ((1ULL << 37));
#else
#error Unsupported architecture
#endif

TEST(FutexTest, FutexFailsWithEFAULTOnLowestNormalAddress) {
  // The restricted / normal address space layout is Starnix-specific.
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP();
  }
  EXPECT_THAT(syscall(SYS_futex, kLowestNormalModeAddress, FUTEX_WAIT, 0, nullptr, 0, 0),
              SyscallFailsWithErrno(EFAULT));
}

TEST(FutexTest, FutexSucceedsHighestRestrictedAddress) {
  // The restricted / normal address space layout is Starnix-specific.
  if (!test_helper::IsStarnix()) {
    GTEST_SKIP();
  }
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGESIZE));
  const uintptr_t highest_restricted_mode_address = kLowestNormalModeAddress - page_size;
  // With only 8 bits of stack ASLR on arch32, this page is sometimes the live stack, so use
  // MAP_FIXED_NOREPLACE: replacing the stack would kill the test. See
  // https://fxbug.dev/563359856.
  void *result =
      mmap(reinterpret_cast<void *>(highest_restricted_mode_address), page_size,
           PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
  if (result == MAP_FAILED && errno == EEXIST) {
    GTEST_SKIP() << "The highest restricted page is already mapped, which happens when stack ASLR "
                    "picks an offset of 0 pages.";
  }
  ASSERT_NE(result, MAP_FAILED) << strerror(errno);
  ASSERT_EQ(highest_restricted_mode_address, reinterpret_cast<uintptr_t>(result));
  struct timespec wait_timeout = {};
  long futex_result =
      syscall(SYS_futex, highest_restricted_mode_address, FUTEX_WAIT, 0, &wait_timeout, 0, 0);
  EXPECT_EQ(futex_result, -1);
  EXPECT_EQ(errno, ETIMEDOUT);
  SAFE_SYSCALL(munmap(reinterpret_cast<void *>(highest_restricted_mode_address), page_size));
}

constexpr size_t kUser1Uid = 65533;

struct GetRobustListTestCase {
  bool same_user;
  bool has_cap;
  int expected_errno;
};

class GetRobustListTest : public ::testing::TestWithParam<GetRobustListTestCase> {};

TEST_P(GetRobustListTest, DifferentUserRequiresCapSysPtrace) {
  const GetRobustListTestCase &params = GetParam();

  test_helper::ForkHelper tracer_helper;

  // Start the tracer process.
  tracer_helper.RunInForkedProcess([&]() {
    test_helper::ForkHelper tracee_helper;

    int pipe_fds[2];
    SAFE_SYSCALL(pipe(pipe_fds));
    fbl::unique_fd wait_fd(pipe_fds[0]);
    fbl::unique_fd signal_fd(pipe_fds[1]);

    // Start the tracee process as a child of the tracer.
    pid_t tracee_pid = tracee_helper.RunInForkedProcess([&]() {
      if (!params.same_user) {
        SAFE_SYSCALL(setresuid(kUser1Uid, kUser1Uid, kUser1Uid));
      }

      struct robust_list_head head;
      SAFE_SYSCALL(syscall(SYS_set_robust_list, &head, sizeof(head)));

      // Signal that the tracee has done initializing.
      char buf = 'a';
      SAFE_SYSCALL(write(signal_fd.get(), &buf, sizeof(buf)));

      // Wait until the tracer is done.
      pause();
    });

    // Wait for the tracee to be ready.
    char buf;
    SAFE_SYSCALL(read(wait_fd.get(), &buf, sizeof(buf)));

    if (!params.has_cap) {
      test_helper::UnsetCapabilityEffective(CAP_SYS_PTRACE);
    }

    // Attempt to read the futex list.
    struct robust_list_head *read_head = nullptr;
    size_t read_len = 0;
    long ret = syscall(SYS_get_robust_list, tracee_pid, &read_head, &read_len);

    if (params.expected_errno == 0) {
      EXPECT_THAT(ret, SyscallSucceeds());
    } else {
      EXPECT_THAT(ret, SyscallFailsWithErrno(params.expected_errno));
    }

    // Teardown the tracee.
    SAFE_SYSCALL(kill(tracee_pid, SIGKILL));
    tracee_helper.ExpectSignal(SIGKILL);

    EXPECT_TRUE(tracee_helper.WaitForChildren());
  });

  EXPECT_TRUE(tracer_helper.WaitForChildren());
}

TEST(FutexTest, EvictedPageContentionUnderMemoryPressure) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    char filename[] = "/tmp/futex_test_XXXXXX";
    int fd = mkstemp(filename);
    ASSERT_GE(fd, 0);
    unlink(filename);

    constexpr size_t kFileSize = 16 * 1024 * 1024;  // 16 MB
    ASSERT_EQ(ftruncate(fd, kFileSize), 0);

    uint32_t *mapped = reinterpret_cast<uint32_t *>(
        mmap(nullptr, kFileSize, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0));
    ASSERT_NE(mapped, MAP_FAILED);

    std::atomic<bool> stop{false};
    std::barrier sync_barrier(2);

    // Thread 1: loops calling FUTEX_WAIT on madvise'd pages
    std::thread t_waiter([&]() {
      size_t offset = 0;
      sync_barrier.arrive_and_wait();
      while (!stop.load()) {
        madvise(&mapped[offset], 4096, MADV_DONTNEED);
        struct timespec ts = {.tv_sec = 0, .tv_nsec = 1000000};  // 1ms
        syscall(SYS_futex, &mapped[offset], FUTEX_WAIT, 0, &ts, nullptr, 0);
        offset = (offset + 1024) % (kFileSize / sizeof(uint32_t));
      }
    });

    sync_barrier.arrive_and_wait();

    // Thread 2: Simulates ActivityManager reading /proc/self/status concurrently
    std::thread t_procfs([&]() {
      char buf[1024];
      while (!stop.load()) {
        int pfd = open("/proc/self/status", O_RDONLY);
        if (pfd >= 0) {
          read(pfd, buf, sizeof(buf));
          close(pfd);
        }
      }
    });

    // Main thread verifies other futex operations proceed without deadlocking
    uint32_t dummy_futex = 0;
    for (int i = 0; i < 200; ++i) {
      syscall(SYS_futex, &dummy_futex, FUTEX_WAKE, 1, nullptr, nullptr, 0);
      usleep(1000);
    }

    stop.store(true);
    t_waiter.join();
    t_procfs.join();
    munmap(mapped, kFileSize);
    close(fd);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(FutexTest, UntimedFutexWaitRestartsOnSignalWithSaRestart) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    g_signal_count.store(0);
    struct sigaction sa = {};
    sa.sa_handler = CountingSignalHandler;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    SAFE_SYSCALL(sigaction(SIGUSR1, &sa, nullptr));

    const pid_t pid = getpid();
    std::atomic<uint32_t> futex_word{0};
    std::atomic<pid_t> waiter_tid{0};
    std::atomic<bool> wait_returned{false};
    std::atomic<long> wait_result{0};
    std::atomic<int> wait_errno{0};

    std::thread waiter([&] {
      waiter_tid.store(static_cast<pid_t>(syscall(SYS_gettid)));
      long ret = syscall(SYS_futex, &futex_word, FUTEX_WAIT, 0, nullptr, nullptr, 0);
      wait_errno.store(errno);
      wait_result.store(ret);
      wait_returned.store(true);
    });
    auto waiter_cleanup = fit::defer([&] {
      if (waiter.joinable()) {
        waiter.detach();
      }
    });

    while (waiter_tid.load() == 0) {
      sched_yield();
    }

    // Wait until the waiter is sleeping in FUTEX_WAIT.
    ASSERT_TRUE(test_helper::WaitForTaskState(pid, waiter_tid.load(),
                                              [](std::string_view state) { return state == "S"; }));

    // Send SIGUSR1 to interrupt the futex wait.
    SAFE_SYSCALL(syscall(SYS_tgkill, pid, waiter_tid.load(), SIGUSR1));

    // Wait until the signal handler has executed.
    while (g_signal_count.load() == 0) {
      sched_yield();
    }

    // Because SA_RESTART is set, the futex wait should restart and remain blocked rather than
    // returning EINTR to userspace.
    ASSERT_TRUE(test_helper::WaitForTaskState(pid, waiter_tid.load(), [&](std::string_view state) {
      if (wait_returned.load()) {
        return false;
      }
      return state == "S";
    }));
    EXPECT_FALSE(wait_returned.load());

    // Wake the futex and confirm the waiter receives it.
    futex_word.store(1);
    long woken = SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_WAKE, 1, nullptr, nullptr, 0));
    EXPECT_EQ(woken, 1);

    waiter.join();
    EXPECT_TRUE(wait_returned.load());
    EXPECT_EQ(wait_result.load(), 0);
    EXPECT_EQ(g_signal_count.load(), 1);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(FutexTest, UntimedFutexWaitPrivateRestartsOnSignalWithSaRestart) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    g_signal_count.store(0);
    struct sigaction sa = {};
    sa.sa_handler = CountingSignalHandler;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    SAFE_SYSCALL(sigaction(SIGUSR1, &sa, nullptr));

    const pid_t pid = getpid();
    std::atomic<uint32_t> futex_word{0};
    std::atomic<pid_t> waiter_tid{0};
    std::atomic<bool> wait_returned{false};
    std::atomic<long> wait_result{0};
    std::atomic<int> wait_errno{0};

    std::thread waiter([&] {
      waiter_tid.store(static_cast<pid_t>(syscall(SYS_gettid)));
      long ret = syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0, nullptr, nullptr, 0);
      wait_errno.store(errno);
      wait_result.store(ret);
      wait_returned.store(true);
    });
    auto waiter_cleanup = fit::defer([&] {
      if (waiter.joinable()) {
        waiter.detach();
      }
    });

    while (waiter_tid.load() == 0) {
      sched_yield();
    }

    // Wait until the waiter is sleeping in FUTEX_WAIT_PRIVATE.
    ASSERT_TRUE(test_helper::WaitForTaskState(pid, waiter_tid.load(),
                                              [](std::string_view state) { return state == "S"; }));

    // Send SIGUSR1 to interrupt the futex wait.
    SAFE_SYSCALL(syscall(SYS_tgkill, pid, waiter_tid.load(), SIGUSR1));

    // Wait until the signal handler has executed.
    while (g_signal_count.load() == 0) {
      sched_yield();
    }

    // Because SA_RESTART is set, the private futex wait should restart and remain blocked rather
    // than returning EINTR to userspace.
    ASSERT_TRUE(test_helper::WaitForTaskState(pid, waiter_tid.load(), [&](std::string_view state) {
      if (wait_returned.load()) {
        return false;
      }
      return state == "S";
    }));
    EXPECT_FALSE(wait_returned.load());

    // Wake the private futex and confirm the waiter receives it.
    futex_word.store(1);
    long woken =
        SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_WAKE_PRIVATE, 1, nullptr, nullptr, 0));
    EXPECT_EQ(woken, 1);

    waiter.join();
    EXPECT_TRUE(wait_returned.load());
    EXPECT_EQ(wait_result.load(), 0);
    EXPECT_EQ(g_signal_count.load(), 1);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(FutexTest, UntimedFutexWaitFailsWithEintrWithoutSaRestart) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    g_signal_count.store(0);
    struct sigaction sa = {};
    sa.sa_handler = CountingSignalHandler;
    sa.sa_flags = 0;  // Deliberately no SA_RESTART.
    sigemptyset(&sa.sa_mask);
    SAFE_SYSCALL(sigaction(SIGUSR1, &sa, nullptr));

    const pid_t pid = getpid();
    std::atomic<uint32_t> futex_word{0};
    std::atomic<pid_t> waiter_tid{0};
    std::atomic<bool> wait_returned{false};
    std::atomic<long> wait_result{0};
    std::atomic<int> wait_errno{0};

    std::thread waiter([&] {
      waiter_tid.store(static_cast<pid_t>(syscall(SYS_gettid)));
      long ret = syscall(SYS_futex, &futex_word, FUTEX_WAIT, 0, nullptr, nullptr, 0);
      wait_errno.store(errno);
      wait_result.store(ret);
      wait_returned.store(true);
    });
    auto waiter_cleanup = fit::defer([&] {
      if (waiter.joinable()) {
        waiter.detach();
      }
    });

    while (waiter_tid.load() == 0) {
      sched_yield();
    }

    ASSERT_TRUE(test_helper::WaitForTaskState(pid, waiter_tid.load(),
                                              [](std::string_view state) { return state == "S"; }));

    SAFE_SYSCALL(syscall(SYS_tgkill, pid, waiter_tid.load(), SIGUSR1));

    waiter.join();
    EXPECT_TRUE(wait_returned.load());
    EXPECT_EQ(wait_result.load(), -1);
    EXPECT_EQ(wait_errno.load(), EINTR);
    EXPECT_EQ(g_signal_count.load(), 1);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(FutexTest, TimedFutexWaitFailsWithEintrEvenWithSaRestart) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    g_signal_count.store(0);
    struct sigaction sa = {};
    sa.sa_handler = CountingSignalHandler;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    SAFE_SYSCALL(sigaction(SIGUSR1, &sa, nullptr));

    const pid_t pid = getpid();
    std::atomic<uint32_t> futex_word{0};
    std::atomic<pid_t> waiter_tid{0};
    std::atomic<bool> wait_returned{false};
    std::atomic<long> wait_result{0};
    std::atomic<int> wait_errno{0};

    std::thread waiter([&] {
      waiter_tid.store(static_cast<pid_t>(syscall(SYS_gettid)));
      struct timespec timeout = {.tv_sec = 60, .tv_nsec = 0};
      long ret = syscall(SYS_futex, &futex_word, FUTEX_WAIT, 0, &timeout, nullptr, 0);
      wait_errno.store(errno);
      wait_result.store(ret);
      wait_returned.store(true);
    });
    auto waiter_cleanup = fit::defer([&] {
      if (waiter.joinable()) {
        waiter.detach();
      }
    });

    while (waiter_tid.load() == 0) {
      sched_yield();
    }

    ASSERT_TRUE(test_helper::WaitForTaskState(pid, waiter_tid.load(),
                                              [](std::string_view state) { return state == "S"; }));

    SAFE_SYSCALL(syscall(SYS_tgkill, pid, waiter_tid.load(), SIGUSR1));

    waiter.join();
    EXPECT_TRUE(wait_returned.load());
    EXPECT_EQ(wait_result.load(), -1);
    EXPECT_EQ(wait_errno.load(), EINTR);
    EXPECT_EQ(g_signal_count.load(), 1);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(FutexTest, TimedFutexWaitBitsetFailsWithEintrEvenWithSaRestart) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    g_signal_count.store(0);
    struct sigaction sa = {};
    sa.sa_handler = CountingSignalHandler;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    SAFE_SYSCALL(sigaction(SIGUSR1, &sa, nullptr));

    const pid_t pid = getpid();
    std::atomic<uint32_t> futex_word{0};
    std::atomic<pid_t> waiter_tid{0};
    std::atomic<bool> wait_returned{false};
    std::atomic<long> wait_result{0};
    std::atomic<int> wait_errno{0};

    std::thread waiter([&] {
      waiter_tid.store(static_cast<pid_t>(syscall(SYS_gettid)));
      struct timespec timeout = {};
      SAFE_SYSCALL(clock_gettime(CLOCK_MONOTONIC, &timeout));
      timeout.tv_sec += 60;
      long ret = syscall(SYS_futex, &futex_word, FUTEX_WAIT_BITSET, 0, &timeout, nullptr,
                         FUTEX_BITSET_MATCH_ANY);
      wait_errno.store(errno);
      wait_result.store(ret);
      wait_returned.store(true);
    });
    auto waiter_cleanup = fit::defer([&] {
      if (waiter.joinable()) {
        waiter.detach();
      }
    });

    while (waiter_tid.load() == 0) {
      sched_yield();
    }

    ASSERT_TRUE(test_helper::WaitForTaskState(pid, waiter_tid.load(),
                                              [](std::string_view state) { return state == "S"; }));

    SAFE_SYSCALL(syscall(SYS_tgkill, pid, waiter_tid.load(), SIGUSR1));

    waiter.join();
    EXPECT_TRUE(wait_returned.load());
    EXPECT_EQ(wait_result.load(), -1);
    EXPECT_EQ(wait_errno.load(), EINTR);
    EXPECT_EQ(g_signal_count.load(), 1);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(FutexTest, UntimedFutexWaitBitsetRestartsOnSignalWithSaRestart) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    g_signal_count.store(0);
    struct sigaction sa = {};
    sa.sa_handler = CountingSignalHandler;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    SAFE_SYSCALL(sigaction(SIGUSR1, &sa, nullptr));

    const pid_t pid = getpid();
    std::atomic<uint32_t> futex_word{0};
    std::atomic<pid_t> waiter_tid{0};
    std::atomic<bool> wait_returned{false};
    std::atomic<long> wait_result{0};
    std::atomic<int> wait_errno{0};

    std::thread waiter([&] {
      waiter_tid.store(static_cast<pid_t>(syscall(SYS_gettid)));
      long ret = syscall(SYS_futex, &futex_word, FUTEX_WAIT_BITSET, 0, nullptr, nullptr,
                         FUTEX_BITSET_MATCH_ANY);
      wait_errno.store(errno);
      wait_result.store(ret);
      wait_returned.store(true);
    });
    auto waiter_cleanup = fit::defer([&] {
      if (waiter.joinable()) {
        waiter.detach();
      }
    });

    while (waiter_tid.load() == 0) {
      sched_yield();
    }

    ASSERT_TRUE(test_helper::WaitForTaskState(pid, waiter_tid.load(),
                                              [](std::string_view state) { return state == "S"; }));

    SAFE_SYSCALL(syscall(SYS_tgkill, pid, waiter_tid.load(), SIGUSR1));

    while (g_signal_count.load() == 0) {
      sched_yield();
    }

    ASSERT_TRUE(test_helper::WaitForTaskState(pid, waiter_tid.load(), [&](std::string_view state) {
      if (wait_returned.load()) {
        return false;
      }
      return state == "S";
    }));
    EXPECT_FALSE(wait_returned.load());

    futex_word.store(1);
    long woken = SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_WAKE_BITSET, 1, nullptr,
                                      nullptr, FUTEX_BITSET_MATCH_ANY));
    EXPECT_EQ(woken, 1);

    waiter.join();
    EXPECT_TRUE(wait_returned.load());
    EXPECT_EQ(wait_result.load(), 0);
    EXPECT_EQ(g_signal_count.load(), 1);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(FutexTest, UntimedFutexWaitRestartsAcrossMultipleSignalsWithSaRestart) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    g_signal_count.store(0);
    struct sigaction sa = {};
    sa.sa_handler = CountingSignalHandler;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    SAFE_SYSCALL(sigaction(SIGUSR1, &sa, nullptr));

    const pid_t pid = getpid();
    std::atomic<uint32_t> futex_word{0};
    std::atomic<pid_t> waiter_tid{0};
    std::atomic<bool> wait_returned{false};
    std::atomic<long> wait_result{0};
    std::atomic<int> wait_errno{0};

    std::thread waiter([&] {
      waiter_tid.store(static_cast<pid_t>(syscall(SYS_gettid)));
      long ret = syscall(SYS_futex, &futex_word, FUTEX_WAIT, 0, nullptr, nullptr, 0);
      wait_errno.store(errno);
      wait_result.store(ret);
      wait_returned.store(true);
    });
    auto waiter_cleanup = fit::defer([&] {
      if (waiter.joinable()) {
        waiter.detach();
      }
    });

    while (waiter_tid.load() == 0) {
      sched_yield();
    }

    ASSERT_TRUE(test_helper::WaitForTaskState(pid, waiter_tid.load(),
                                              [](std::string_view state) { return state == "S"; }));

    for (int i = 1; i <= 3; ++i) {
      SAFE_SYSCALL(syscall(SYS_tgkill, pid, waiter_tid.load(), SIGUSR1));
      while (g_signal_count.load() < i) {
        sched_yield();
      }
      ASSERT_TRUE(
          test_helper::WaitForTaskState(pid, waiter_tid.load(), [&](std::string_view state) {
            if (wait_returned.load()) {
              return false;
            }
            return state == "S";
          }));
      EXPECT_FALSE(wait_returned.load());
    }

    futex_word.store(1);
    long woken = SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_WAKE, 1, nullptr, nullptr, 0));
    EXPECT_EQ(woken, 1);

    waiter.join();
    EXPECT_TRUE(wait_returned.load());
    EXPECT_EQ(wait_result.load(), 0);
    EXPECT_EQ(g_signal_count.load(), 3);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(FutexTest, UntimedFutexWaitReturnsEagainIfValueChangedDuringSignalHandler) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    std::atomic<uint32_t> futex_word{0};
    g_signal_count.store(0);
    g_signal_futex_word.store(&futex_word, std::memory_order_relaxed);
    auto futex_word_cleanup =
        fit::defer([] { g_signal_futex_word.store(nullptr, std::memory_order_relaxed); });

    struct sigaction sa = {};
    sa.sa_handler = ValueModifyingSignalHandler;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    SAFE_SYSCALL(sigaction(SIGUSR1, &sa, nullptr));

    const pid_t pid = getpid();
    std::atomic<pid_t> waiter_tid{0};
    std::atomic<bool> wait_returned{false};
    std::atomic<long> wait_result{0};
    std::atomic<int> wait_errno{0};

    std::thread waiter([&] {
      waiter_tid.store(static_cast<pid_t>(syscall(SYS_gettid)));
      long ret = syscall(SYS_futex, &futex_word, FUTEX_WAIT, 0, nullptr, nullptr, 0);
      wait_errno.store(errno);
      wait_result.store(ret);
      wait_returned.store(true);
    });
    auto waiter_cleanup = fit::defer([&] {
      if (waiter.joinable()) {
        waiter.detach();
      }
    });

    while (waiter_tid.load() == 0) {
      sched_yield();
    }

    ASSERT_TRUE(test_helper::WaitForTaskState(pid, waiter_tid.load(),
                                              [](std::string_view state) { return state == "S"; }));

    // Send SIGUSR1. The signal handler will modify futex_word to 1.
    SAFE_SYSCALL(syscall(SYS_tgkill, pid, waiter_tid.load(), SIGUSR1));

    // Waiter should return with EAGAIN because upon restart the futex word no longer matches 0.
    waiter.join();
    EXPECT_TRUE(wait_returned.load());
    EXPECT_EQ(wait_result.load(), -1);
    EXPECT_EQ(wait_errno.load(), EAGAIN);
    EXPECT_EQ(g_signal_count.load(), 1);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

std::optional<long> GetThreadProcStatPriority(pid_t tid) {
  std::string content;
  if (!files::ReadFileToString("/proc/" + std::to_string(tid) + "/stat", &content)) {
    return std::nullopt;
  }
  size_t rparen = content.rfind(')');
  if (rparen == std::string::npos || rparen + 2 >= content.size()) {
    return std::nullopt;
  }
  std::istringstream iss(content.substr(rparen + 2));
  std::string token;
  // Field 3 (state) is index 0 after ") "; field 18 (priority) is index 15.
  for (int i = 0; i <= 15; ++i) {
    if (!(iss >> token)) {
      return std::nullopt;
    }
  }
  char *end = nullptr;
  long value = strtol(token.c_str(), &end, 10);
  if (end == token.c_str() || *end != '\0') {
    return std::nullopt;
  }
  return value;
}

// Polls field 18 of /proc/<tid>/stat until it equals `expected` or a deadline passes, and returns
// the last value read. A thread that is merely sleeping is not proof that it is queued on a PI
// futex, so tests wait on the boost itself.
std::optional<long> WaitForThreadProcStatPriority(pid_t tid, long expected) {
  const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(30);
  std::optional<long> priority = GetThreadProcStatPriority(tid);
  while (priority != expected && std::chrono::steady_clock::now() < deadline) {
    std::this_thread::sleep_for(std::chrono::milliseconds(10));
    priority = GetThreadProcStatPriority(tid);
  }
  return priority;
}

// Polls a PI futex word until the kernel sets FUTEX_WAITERS, which it does only once another
// thread is blocking in FUTEX_LOCK_PI on it. Returns false if a deadline passes first.
bool WaitForFutexWaitersBit(const std::atomic<uint32_t> &futex_word) {
  const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(30);
  while ((futex_word.load() & FUTEX_WAITERS) == 0) {
    if (std::chrono::steady_clock::now() >= deadline) {
      return false;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(1));
  }
  return true;
}

TEST(FutexTest, LockPiBoostsOwnerPriorityInProcStat) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    struct sigaction sa = {};
    sa.sa_handler = CountingSignalHandler;
    sa.sa_flags = 0;
    sigemptyset(&sa.sa_mask);
    SAFE_SYSCALL(sigaction(SIGUSR1, &sa, nullptr));

    const pid_t pid = getpid();
    const pid_t high_tid = static_cast<pid_t>(syscall(SYS_gettid));
    SAFE_SYSCALL(setpriority(PRIO_PROCESS, high_tid, 0));
    ASSERT_EQ(GetThreadProcStatPriority(high_tid), 20);

    std::atomic<uint32_t> futex_word{0};
    std::atomic<pid_t> owner_tid_published{0};
    std::atomic<pid_t> mid_tid_published{0};
    std::latch owner_locked(1);
    std::latch low_ready(1);
    std::latch mid_queuing(1);
    std::latch high_queuing(1);

    std::thread owner([&] {
      const pid_t owner_tid = static_cast<pid_t>(syscall(SYS_gettid));
      owner_tid_published.store(owner_tid);
      SAFE_SYSCALL(setpriority(PRIO_PROCESS, owner_tid, 10));
      EXPECT_EQ(GetThreadProcStatPriority(owner_tid), 30);

      SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_LOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));
      EXPECT_EQ(futex_word.load(), static_cast<uint32_t>(owner_tid));
      owner_locked.count_down();

      // Wait until `low_waiter`, `mid_waiter`, and `high_tid` are all queued on `futex_word`
      // concurrently while `owner` holds the lock.
      high_queuing.wait();
      EXPECT_TRUE(test_helper::WaitForTaskState(
          pid, high_tid, [](std::string_view state) { return state == "S"; }));
      EXPECT_EQ(GetThreadProcStatPriority(owner_tid), 25);

      SAFE_SYSCALL(
          syscall(SYS_futex, &futex_word, FUTEX_UNLOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));
      EXPECT_EQ(GetThreadProcStatPriority(owner_tid), 30);
    });

    owner_locked.wait();
    const pid_t owner_tid = owner_tid_published.load();
    EXPECT_THAT(syscall(SYS_futex, &futex_word, FUTEX_UNLOCK_PI_PRIVATE, 0, nullptr, nullptr, 0),
                SyscallFailsWithErrno(EPERM));

    std::thread low_waiter([&] {
      const pid_t tid = static_cast<pid_t>(syscall(SYS_gettid));
      SAFE_SYSCALL(setpriority(PRIO_PROCESS, tid, 5));
      EXPECT_EQ(GetThreadProcStatPriority(tid), 25);
      low_ready.count_down();

      SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_LOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));
      // `mid_waiter` and `high_tid` were already queued when `owner` unlocked, so `FUTEX_WAITERS`
      // remains set and their `owner_tid` was transferred to `low_waiter`. Interrupt `mid_waiter`
      // so `high_tid` (which was already queued during `owner`'s unlock) becomes the top waiter
      // on `low_waiter`, boosting `low_waiter` from 25 to 20.
      EXPECT_EQ(futex_word.load(), static_cast<uint32_t>(tid) | FUTEX_WAITERS);
      EXPECT_EQ(GetThreadProcStatPriority(tid), 25);
      SAFE_SYSCALL(syscall(SYS_tgkill, pid, mid_tid_published.load(), SIGUSR1));
      EXPECT_EQ(WaitForThreadProcStatPriority(tid, 20), 20);

      SAFE_SYSCALL(
          syscall(SYS_futex, &futex_word, FUTEX_UNLOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));
      EXPECT_EQ(GetThreadProcStatPriority(tid), 25);
    });

    // Wait until `low_waiter` is queued on `futex_word` and boosting `owner` to 25.
    low_ready.wait();
    EXPECT_TRUE(WaitForFutexWaitersBit(futex_word));
    EXPECT_EQ(WaitForThreadProcStatPriority(owner_tid, 25), 25);

    std::thread mid_waiter([&] {
      const pid_t tid = static_cast<pid_t>(syscall(SYS_gettid));
      mid_tid_published.store(tid);
      SAFE_SYSCALL(setpriority(PRIO_PROCESS, tid, 5));
      mid_queuing.count_down();
      long ret = syscall(SYS_futex, &futex_word, FUTEX_LOCK_PI_PRIVATE, 0, nullptr, nullptr, 0);
      if (ret == 0) {
        SAFE_SYSCALL(
            syscall(SYS_futex, &futex_word, FUTEX_UNLOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));
      } else {
        EXPECT_EQ(errno, EINTR);
      }
    });

    mid_queuing.wait();
    ASSERT_TRUE(test_helper::WaitForTaskState(pid, mid_tid_published.load(),
                                              [](std::string_view state) { return state == "S"; }));

    high_queuing.count_down();
    SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_LOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));
    EXPECT_EQ(futex_word.load() & FUTEX_TID_MASK, static_cast<uint32_t>(high_tid));
    SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_UNLOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));

    mid_waiter.join();
    low_waiter.join();
    owner.join();
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

TEST(FutexTest, LockPiRealtimeWaiterBoostsOwnerPriorityInProcStat) {
  if (!test_helper::HasCapability(CAP_SYS_NICE)) {
    GTEST_SKIP() << "Setting a real-time scheduling policy requires CAP_SYS_NICE.";
  }
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    const pid_t owner_tid = static_cast<pid_t>(syscall(SYS_gettid));
    SAFE_SYSCALL(setpriority(PRIO_PROCESS, owner_tid, 0));
    ASSERT_EQ(GetThreadProcStatPriority(owner_tid), 20);

    // Real-time tasks report -1 - rt_priority.
    for (int rt_priority : {1, 99}) {
      sched_param param = {.sched_priority = rt_priority};
      SAFE_SYSCALL(sched_setscheduler(owner_tid, SCHED_FIFO, &param));
      EXPECT_EQ(GetThreadProcStatPriority(owner_tid), -1 - rt_priority);
    }
    sched_param normal_param = {.sched_priority = 0};
    SAFE_SYSCALL(sched_setscheduler(owner_tid, SCHED_OTHER, &normal_param));
    ASSERT_EQ(GetThreadProcStatPriority(owner_tid), 20);

    std::atomic<uint32_t> futex_word{0};
    SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_LOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));
    ASSERT_EQ(futex_word.load(), static_cast<uint32_t>(owner_tid));

    std::atomic<int> acquire_order{0};
    int normal_order = 0;
    int rt_order = 0;

    std::thread normal_waiter([&] {
      const pid_t tid = static_cast<pid_t>(syscall(SYS_gettid));
      SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_LOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));
      EXPECT_EQ(futex_word.load() & FUTEX_TID_MASK, static_cast<uint32_t>(tid));
      normal_order = ++acquire_order;
      SAFE_SYSCALL(
          syscall(SYS_futex, &futex_word, FUTEX_UNLOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));
    });

    // Wait until `normal_waiter` is queued on `futex_word` before spawning `rt_waiter`.
    ASSERT_TRUE(WaitForFutexWaitersBit(futex_word));

    std::thread rt_waiter([&] {
      const pid_t tid = static_cast<pid_t>(syscall(SYS_gettid));
      sched_param param = {.sched_priority = 1};
      SAFE_SYSCALL(sched_setscheduler(tid, SCHED_FIFO, &param));
      EXPECT_EQ(GetThreadProcStatPriority(tid), -2);

      SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_LOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));
      EXPECT_EQ(futex_word.load(), static_cast<uint32_t>(tid) | FUTEX_WAITERS);
      rt_order = ++acquire_order;
      SAFE_SYSCALL(
          syscall(SYS_futex, &futex_word, FUTEX_UNLOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));
    });

    // The SCHED_OTHER owner inherits the real-time waiter's priority while it is queued.
    EXPECT_EQ(WaitForThreadProcStatPriority(owner_tid, -2), -2);

    SAFE_SYSCALL(syscall(SYS_futex, &futex_word, FUTEX_UNLOCK_PI_PRIVATE, 0, nullptr, nullptr, 0));
    EXPECT_EQ(GetThreadProcStatPriority(owner_tid), 20);

    rt_waiter.join();
    normal_waiter.join();
    EXPECT_EQ(rt_order, 1);
    EXPECT_EQ(normal_order, 2);
  });
  EXPECT_TRUE(helper.WaitForChildren());
}

INSTANTIATE_TEST_SUITE_P(
    GetRobustListTest, GetRobustListTest,
    ::testing::Values(
        GetRobustListTestCase{.same_user = true, .has_cap = false, .expected_errno = 0},
        GetRobustListTestCase{.same_user = false, .has_cap = true, .expected_errno = 0},
        GetRobustListTestCase{.same_user = false, .has_cap = false, .expected_errno = EPERM}));

}  // anonymous namespace
