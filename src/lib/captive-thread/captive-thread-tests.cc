// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/captive-thread/captive-thread.h>
#include <lib/captive-thread/testing/matchers.h>

#include <cstdint>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

namespace {

using ::testing::_;
using ::testing::AllOf;
using ::testing::Eq;
using ::testing::Field;
using ::testing::IsSupersetOf;
using ::testing::Ne;
using ::testing::Not;
using ::testing::Pair;

// Do a bit cast and also hide the value from the compiler so it cannot be
// constant-folded in callers.
template <typename T>
  requires(sizeof(T) <= sizeof(uintptr_t))
T LaunderAs(auto value)
  requires(sizeof(value) <= sizeof(uintptr_t))
{
  T result;
  __asm__("" : "=r"(result) : "0"(value));
  return result;
}

// This always crashes and never returns, but the compiler doesn't know that.
// In fact, it can return if the PC is advanced past the trap instruction.
void Crash() {
  // This is the same instruction that __builtin_trap() emits.  But the
  // compiler knows that __builtin_trap() cannot return and so it may decide to
  // move its instruction out of the straight-ahead path even if it's hidden
  // that the trap path is always taken.
  __asm__ volatile(
#ifdef __aarch64__
      "brk #1"
#elifdef __riscv
      "unimp"
#elifdef __x86_64__
      "ud2"
#endif
  );
}

template <uintptr_t Address = 0>
[[clang::no_sanitize("all")]] void PageFault() {
  *LaunderAs<volatile int*>(Address) = 0;
}

TEST(CaptiveThreadTests, Destroy) {
  {
    // Destroyed immediately after construction.
    captive_thread::CaptiveThread thread([] {});
  }
}

TEST(CaptiveThreadTests, ExplicitJoin) {
  captive_thread::CaptiveThread thread([] {});
  EXPECT_FALSE(thread.Joined());
  thread.ForceJoin();
  EXPECT_TRUE(thread.Joined());
}

TEST(CaptiveThreadTests, BlockUntilSuccess) {
  bool ran = false;
  {
    captive_thread::CaptiveThread thread([&ran] { ran = true; });
    thread.BlockUntilSuccess();
    EXPECT_TRUE(thread.Joined());
  }
  EXPECT_TRUE(ran);
}

TEST(CaptiveThreadTests, CrashAndJoin) {
  captive_thread::CaptiveThread thread{Crash};
  thread.ForceJoin();
}

TEST(CaptiveThreadTests, WaitForException) {
  bool started = false, finished = false;
  {
    captive_thread::CaptiveThread thread{[&started, &finished] {
      started = true;
      Crash();
      finished = true;  // Should not be reached, but the compiler won't know.
    }};
    zx::result result = thread.WaitForException();
    EXPECT_TRUE(result.is_ok()) << result.status_value();
    EXPECT_FALSE(thread.Joined());
    EXPECT_TRUE(thread.InException());
    EXPECT_TRUE(thread.IsStopped());
  }
  EXPECT_TRUE(started);
  EXPECT_FALSE(finished);
}

TEST(CaptiveThreadTests, ExceptionReport) {
  captive_thread::CaptiveThread thread{Crash};
  zx::result result = thread.WaitForException();
  EXPECT_TRUE(result.is_ok()) << result.status_value();
  EXPECT_THAT(thread.ExceptionReport(),
              Optional(Field(&zx_exception_report_t::header,
                             Field(&zx_exception_header_t::type, captive_thread::kTrapException))));
}

TEST(CaptiveThreadTests, Suspend) {
  captive_thread::CaptiveThread thread{[] {
    while (true) {
      zx::nanosleep(zx::time::infinite());
    }
  }};
  zx::result suspend_result = thread.Suspend();
  ASSERT_TRUE(suspend_result.is_ok()) << suspend_result.status_value();
  EXPECT_TRUE(thread.InSuspend());
  EXPECT_TRUE(thread.IsStopped());
  EXPECT_FALSE(thread.InException());
  zx::result wait_result = thread.WaitForStop();
  EXPECT_TRUE(wait_result.is_ok()) << wait_result.status_value();
}

TEST(CaptiveThreadTests, SuspendVsException) {
  captive_thread::CaptiveThread thread{Crash};

  // It might already have an exception pending, but we haven't checked yet, so
  // Suspend() will always start a suspension.
  zx::result suspend_result = thread.Suspend();
  ASSERT_TRUE(suspend_result.is_ok()) << suspend_result.status_value();
  EXPECT_TRUE(thread.InSuspend());
  EXPECT_TRUE(thread.IsStopped());

  // This might get either the exception or the suspension first.
  zx::result wait_result = thread.WaitForStop();
  EXPECT_TRUE(wait_result.is_ok()) << wait_result.status_value();

  // Either way, the suspension is still in place.
  EXPECT_TRUE(thread.IsStopped());
  EXPECT_TRUE(thread.InSuspend());
}

TEST(CaptiveThreadTests, Matchers) {
  using captive_thread::testing::IsPageFault;
  {
    captive_thread::CaptiveThread thread{Crash};
    EXPECT_THAT(thread.WaitForException(), GotException(captive_thread::testing::IsTrap()));
    EXPECT_THAT(thread.WaitForException(), captive_thread::testing::IsTrap());
  }
  {
    captive_thread::CaptiveThread thread{PageFault};
    EXPECT_THAT(thread.WaitForException(), GotException(IsPageFault()));
    EXPECT_THAT(thread.WaitForException(), IsPageFault());
    EXPECT_THAT(thread.WaitForException(), GotException(IsPageFault(0)));
  }
  {
    captive_thread::CaptiveThread thread{PageFault<0x123>};
    EXPECT_THAT(thread.WaitForException(), IsPageFault(Ne(0)));
    EXPECT_THAT(thread.WaitForException(), IsPageFault(Eq(0x123)));
  }
  {
    // Basic death test usage.
    using captive_thread::CaptiveThread;
    using captive_thread::testing::GotException;
    EXPECT_THAT(CaptiveThread(Crash).WaitForException(), GotException());
    EXPECT_THAT(CaptiveThread([] {}).WaitForException(), Not(GotException()));
  }
}

TEST(CaptiveThreadTests, ResolveException) {
  bool started = true, finished = false;
  captive_thread::CaptiveThread thread{[&started, &finished] {
    started = true;
    Crash();
    finished = true;
  }};
  ASSERT_THAT(thread.WaitForException(), AllOf(captive_thread::testing::IsTrap()));
  EXPECT_TRUE(started);
  EXPECT_FALSE(finished);
  zx_thread_state_general_regs_t regs;
  zx::result result = zx::make_result(
      thread.thread_handle()->read_state(ZX_THREAD_STATE_GENERAL_REGS, &regs, sizeof(regs)));
  ASSERT_TRUE(result.is_ok()) << result.status_value();
#ifdef __x86_64__
  regs.rip += captive_thread::kTrapInstructionSize;
#else
  regs.pc += captive_thread::kTrapInstructionSize;
#endif
  result = zx::make_result(
      thread.thread_handle()->write_state(ZX_THREAD_STATE_GENERAL_REGS, &regs, sizeof(regs)));
  ASSERT_TRUE(result.is_ok()) << result.status_string();
  thread.ResolveException();
  EXPECT_FALSE(thread.IsStopped());
  ASSERT_THAT(thread.WaitForException(), Not(captive_thread::testing::GotException()));
  ASSERT_FALSE(thread.IsStopped());
  thread.BlockUntilSuccess();
  EXPECT_TRUE(finished);
}

}  // namespace
