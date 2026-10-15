// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/fit/defer.h>
#include <lib/uart-gate.h>
#include <lib/unittest/unittest.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <arch/arch_ops.h>
#include <kernel/thread.h>
#include <ktl/array.h>
#include <ktl/atomic.h>

// Provides tests with access to UartGate internals.
struct UartGateTestAccess {
  // Returns true if the gate's drained event was signaled, consuming the
  // signal.  Does not block.
  static bool ConsumeDrainedSignal(UartGate& gate) {
    return gate.drained_.Wait(Deadline::infinite_past()) == ZX_OK;
  }
};

namespace {

bool InitialStateTest() {
  BEGIN_TEST;

  UartGate gate;
  EXPECT_EQ(UartGate::Phase::kRunning, gate.phase());

  ASSERT_TRUE(gate.TryEnter());
  EXPECT_EQ(UartGate::Phase::kRunning, gate.phase());
  gate.Exit();

  ASSERT_TRUE(gate.TryEnterUnlessSuspended());
  EXPECT_EQ(UartGate::Phase::kRunning, gate.phase());
  gate.Exit();

  END_TEST;
}

bool DrainWithNoActiveUsersTest() {
  BEGIN_TEST;

  UartGate gate;
  gate.BeginDrain();
  EXPECT_EQ(UartGate::Phase::kDraining, gate.phase());

  // Normal operations are refused while draining.
  EXPECT_FALSE(gate.TryEnter());

  // With no active users, waiting for there to be none returns immediately.
  EXPECT_OK(gate.WaitForNoActiveUsers());

  // An operation may enter and exit while draining (e.g. the IRQ handler).
  // Its exit leaves a pending wakeup, which a subsequent wait consumes.
  ASSERT_TRUE(gate.TryEnterUnlessSuspended());
  EXPECT_FALSE(gate.TryFinishDrain());
  gate.Exit();
  EXPECT_OK(gate.WaitForNoActiveUsers());

  // With no active users, finishing the drain succeeds immediately.
  ASSERT_TRUE(gate.TryFinishDrain());
  EXPECT_EQ(UartGate::Phase::kSuspended, gate.phase());

  // Once suspended, all operations are refused.
  EXPECT_FALSE(gate.TryEnter());
  EXPECT_FALSE(gate.TryEnterUnlessSuspended());
  EXPECT_FALSE(gate.TryFinishDrain());

  // Resuming reopens the gate.
  gate.Resume();
  EXPECT_EQ(UartGate::Phase::kRunning, gate.phase());
  ASSERT_TRUE(gate.TryEnter());
  gate.Exit();

  END_TEST;
}

bool DrainWaitsForActiveUsersTest() {
  BEGIN_TEST;

  UartGate gate;

  // Enter multiple times before draining starts.
  ASSERT_TRUE(gate.TryEnter());
  ASSERT_TRUE(gate.TryEnter());
  ASSERT_TRUE(gate.TryEnterUnlessSuspended());

  gate.BeginDrain();
  EXPECT_EQ(UartGate::Phase::kDraining, gate.phase());

  // Normal entry is refused while draining, and the drain cannot finish while
  // active users remain.
  EXPECT_FALSE(gate.TryEnter());
  EXPECT_FALSE(gate.TryFinishDrain());

  // Operations required to complete in-flight work may still enter while
  // draining.
  ASSERT_TRUE(gate.TryEnterUnlessSuspended());
  EXPECT_FALSE(gate.TryFinishDrain());

  // Exit three of the four active users; one remains.
  gate.Exit();
  EXPECT_FALSE(gate.TryFinishDrain());
  gate.Exit();
  EXPECT_FALSE(gate.TryFinishDrain());
  gate.Exit();
  EXPECT_FALSE(gate.TryFinishDrain());

  // Exiting the final active user allows the drain to finish.
  gate.Exit();
  ASSERT_TRUE(gate.TryFinishDrain());
  EXPECT_EQ(UartGate::Phase::kSuspended, gate.phase());

  EXPECT_FALSE(gate.TryEnter());
  EXPECT_FALSE(gate.TryEnterUnlessSuspended());

  // Verify that the gate can be resumed and suspended again.
  gate.Resume();
  EXPECT_EQ(UartGate::Phase::kRunning, gate.phase());
  gate.BeginDrain();
  ASSERT_TRUE(gate.TryFinishDrain());
  gate.Resume();
  EXPECT_EQ(UartGate::Phase::kRunning, gate.phase());

  END_TEST;
}

bool CancelDrainTest() {
  BEGIN_TEST;

  UartGate gate;

  // Cancel with an active user.  The user is unaffected, and normal entry is
  // allowed again.
  ASSERT_TRUE(gate.TryEnter());
  EXPECT_TRUE(gate.HasActiveUsers());
  gate.BeginDrain();
  EXPECT_FALSE(gate.TryEnter());
  gate.CancelDrain();
  EXPECT_EQ(UartGate::Phase::kRunning, gate.phase());
  EXPECT_TRUE(gate.HasActiveUsers());
  ASSERT_TRUE(gate.TryEnter());
  gate.Exit();
  gate.Exit();
  EXPECT_FALSE(gate.HasActiveUsers());

  // Cancel with no active users.
  gate.BeginDrain();
  EXPECT_FALSE(gate.HasActiveUsers());
  gate.CancelDrain();
  EXPECT_EQ(UartGate::Phase::kRunning, gate.phase());

  // The gate can still be suspended and resumed afterwards.
  gate.BeginDrain();
  ASSERT_TRUE(gate.TryFinishDrain());
  EXPECT_EQ(UartGate::Phase::kSuspended, gate.phase());
  gate.Resume();
  EXPECT_EQ(UartGate::Phase::kRunning, gate.phase());

  END_TEST;
}

// Note that because WaitForNoActiveUsers also polls, this test would pass even
// if the last exit did not wake the waiter.  See LastExitSignalsTest, which
// verifies the signal directly.
bool WaitIsWokenByLastExitTest() {
  BEGIN_TEST;

  struct TestState {
    UartGate gate;
    ktl::atomic<bool> done{false};
    zx_status_t status{ZX_ERR_INTERNAL};
  };
  TestState state;

  // Two users enter before draining starts.
  ASSERT_TRUE(state.gate.TryEnter());
  ASSERT_TRUE(state.gate.TryEnter());
  state.gate.BeginDrain();

  thread_start_routine waiter = [](void* arg) -> int {
    auto* s = static_cast<TestState*>(arg);
    s->status = s->gate.WaitForNoActiveUsers();
    s->done.store(true, ktl::memory_order_release);
    return 0;
  };
  Thread* t = Thread::Create("uart_gate_test_waiter", waiter, &state, DEFAULT_PRIORITY);
  ASSERT_NONNULL(t);
  auto cleanup = fit::defer([&]() {
    // Make sure the waiter can finish, even if we fail part way through.
    while (state.gate.HasActiveUsers()) {
      state.gate.Exit();
    }
    t->Join(nullptr, ZX_TIME_INFINITE);
  });
  t->Resume();

  // Give the waiter a chance to block.  It must not finish while users remain,
  // including after a non-final exit.
  Thread::Current::SleepRelative(ZX_MSEC(1));
  EXPECT_FALSE(state.done.load(ktl::memory_order_acquire));
  state.gate.Exit();
  Thread::Current::SleepRelative(ZX_MSEC(1));
  EXPECT_FALSE(state.done.load(ktl::memory_order_acquire));

  // The final exit wakes the waiter.
  state.gate.Exit();
  cleanup.cancel();
  ASSERT_OK(t->Join(nullptr, ZX_TIME_INFINITE));
  EXPECT_TRUE(state.done.load(ktl::memory_order_acquire));
  EXPECT_OK(state.status);

  ASSERT_TRUE(state.gate.TryFinishDrain());
  state.gate.Resume();

  END_TEST;
}

// Verifies, without relying on timing, that only the last exit while draining
// (via Exit, not ExitWithoutWaking) signals the suspending thread.
bool LastExitSignalsTest() {
  BEGIN_TEST;

  UartGate gate;

  // Exiting while running does not signal.
  ASSERT_TRUE(gate.TryEnter());
  gate.Exit();
  EXPECT_FALSE(UartGateTestAccess::ConsumeDrainedSignal(gate));

  // Only the last exit while draining signals.
  ASSERT_TRUE(gate.TryEnter());
  ASSERT_TRUE(gate.TryEnter());
  gate.BeginDrain();
  gate.Exit();
  EXPECT_FALSE(UartGateTestAccess::ConsumeDrainedSignal(gate));
  gate.Exit();
  EXPECT_TRUE(UartGateTestAccess::ConsumeDrainedSignal(gate));
  EXPECT_FALSE(UartGateTestAccess::ConsumeDrainedSignal(gate));
  gate.CancelDrain();

  // ExitWithoutWaking never signals, even as the last exit while draining.
  ASSERT_TRUE(gate.TryEnter());
  gate.BeginDrain();
  gate.ExitWithoutWaking();
  EXPECT_FALSE(UartGateTestAccess::ConsumeDrainedSignal(gate));
  ASSERT_TRUE(gate.TryFinishDrain());
  gate.Resume();

  END_TEST;
}

// If the last user exits via ExitWithoutWaking, the waiter must still notice (after at most about
// one poll interval) rather than blocking indefinitely.
bool WaitNoticesLastExitWithoutWakingTest() {
  BEGIN_TEST;

  struct TestState {
    UartGate gate;
    zx_status_t status{ZX_ERR_INTERNAL};
  };
  TestState state;

  ASSERT_TRUE(state.gate.TryEnter());
  state.gate.BeginDrain();

  thread_start_routine waiter = [](void* arg) -> int {
    auto* s = static_cast<TestState*>(arg);
    s->status = s->gate.WaitForNoActiveUsers();
    return 0;
  };
  Thread* t = Thread::Create("uart_gate_test_waiter", waiter, &state, DEFAULT_PRIORITY);
  ASSERT_NONNULL(t);
  t->Resume();

  // Give the waiter a chance to block, then exit without waking it.
  Thread::Current::SleepRelative(ZX_MSEC(1));
  state.gate.ExitWithoutWaking();

  ASSERT_OK(t->Join(nullptr, ZX_TIME_INFINITE));
  EXPECT_OK(state.status);

  ASSERT_TRUE(state.gate.TryFinishDrain());
  state.gate.Resume();

  END_TEST;
}

bool ConcurrentEnterExitAndSuspendResumeTest() {
  BEGIN_TEST;

  struct TestState {
    UartGate gate;
    ktl::atomic<uint32_t> active_count{0};
    ktl::atomic<bool> suspended{false};
    ktl::atomic<bool> stop{false};
    ktl::atomic<bool> saw_violation{false};
  };

  TestState state;

  static constexpr auto worker_fn = [](void* arg, bool unless_suspended) -> int {
    auto* s = static_cast<TestState*>(arg);
    while (!s->stop.load(ktl::memory_order_relaxed)) {
      const bool entered =
          unless_suspended ? s->gate.TryEnterUnlessSuspended() : s->gate.TryEnter();
      if (entered) {
        s->active_count.fetch_add(1, ktl::memory_order_relaxed);
        if (s->suspended.load(ktl::memory_order_relaxed) ||
            s->gate.phase() == UartGate::Phase::kSuspended) {
          s->saw_violation.store(true, ktl::memory_order_relaxed);
        }
        arch::Yield();
        if (s->suspended.load(ktl::memory_order_relaxed) ||
            s->gate.phase() == UartGate::Phase::kSuspended) {
          s->saw_violation.store(true, ktl::memory_order_relaxed);
        }
        s->active_count.fetch_sub(1, ktl::memory_order_release);
        s->gate.Exit();
      }
      // Spend some time outside the gate so that the drain is not starved by
      // workers which may enter while draining.
      Thread::Current::SleepRelative(ZX_USEC(10));
    }
    return 0;
  };

  thread_start_routine normal_worker = [](void* arg) -> int { return worker_fn(arg, false); };
  thread_start_routine drain_worker = [](void* arg) -> int { return worker_fn(arg, true); };

  constexpr size_t kNumWorkers = 4;
  ktl::array<Thread*, kNumWorkers> workers{};
  auto cleanup = fit::defer([&]() {
    state.stop.store(true, ktl::memory_order_relaxed);
    for (Thread* t : workers) {
      if (t != nullptr) {
        t->Join(nullptr, ZX_TIME_INFINITE);
      }
    }
  });

  for (size_t i = 0; i < kNumWorkers; ++i) {
    workers[i] =
        Thread::Create("uart_gate_test_worker", (i % 2 == 0) ? normal_worker : drain_worker, &state,
                       DEFAULT_PRIORITY);
    ASSERT_NONNULL(workers[i]);
    workers[i]->Resume();
  }

  constexpr size_t kNumCycles = 20;
  for (size_t cycle = 0; cycle < kNumCycles; ++cycle) {
    Thread::Current::SleepRelative(ZX_USEC(100));

    state.gate.BeginDrain();
    // A lost wakeup would cause this to hang.
    do {
      ASSERT_OK(state.gate.WaitForNoActiveUsers());
    } while (!state.gate.TryFinishDrain());

    EXPECT_EQ(UartGate::Phase::kSuspended, state.gate.phase());
    EXPECT_EQ(0u, state.active_count.load(ktl::memory_order_acquire));

    state.suspended.store(true, ktl::memory_order_relaxed);
    Thread::Current::SleepRelative(ZX_USEC(100));
    EXPECT_EQ(0u, state.active_count.load(ktl::memory_order_relaxed));

    state.suspended.store(false, ktl::memory_order_release);
    state.gate.Resume();
  }

  state.stop.store(true, ktl::memory_order_relaxed);
  for (Thread*& t : workers) {
    ASSERT_OK(t->Join(nullptr, ZX_TIME_INFINITE));
    t = nullptr;
  }

  EXPECT_FALSE(state.saw_violation.load(ktl::memory_order_relaxed));
  EXPECT_EQ(0u, state.active_count.load(ktl::memory_order_relaxed));

  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(uart_gate_tests)
UNITTEST("initial_state", InitialStateTest)
UNITTEST("drain_with_no_active_users", DrainWithNoActiveUsersTest)
UNITTEST("drain_waits_for_active_users", DrainWaitsForActiveUsersTest)
UNITTEST("cancel_drain", CancelDrainTest)
UNITTEST("wait_is_woken_by_last_exit", WaitIsWokenByLastExitTest)
UNITTEST("last_exit_signals", LastExitSignalsTest)
UNITTEST("wait_notices_last_exit_without_waking", WaitNoticesLastExitWithoutWakingTest)
UNITTEST("concurrent_enter_exit_and_suspend_resume", ConcurrentEnterExitAndSuspendResumeTest)
UNITTEST_END_TESTCASE(uart_gate_tests, "uart-gate", "UartGate tests")
