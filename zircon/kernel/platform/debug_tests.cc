// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/debuglog.h>
#include <lib/fit/defer.h>
#include <lib/io.h>
#include <lib/kconcurrent/chainlock_transaction.h>
#include <lib/unittest/unittest.h>
#include <platform.h>
#include <stdio.h>
#include <zircon/errors.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <kernel/thread.h>
#include <ktl/algorithm.h>
#include <ktl/atomic.h>
#include <ktl/string_view.h>
#include <platform/debug.h>

namespace {

// Wait until |thread| has blocked.
void WaitUntilBlocked(Thread* thread) {
  zx_duration_mono_t wait_duration = ZX_USEC(1);
  while (true) {
    {
      SingleChainLockGuard guard{IrqSaveOption, thread->get_lock(), CLT_TAG("WaitUntilBlocked")};
      if (thread->state() == THREAD_BLOCKED) {
        return;
      }
    }
    Thread::Current::SleepRelative(wait_duration);
    wait_duration = ktl::min(wait_duration * 2, ZX_MSEC(10));
  }
}

struct WriterState {
  ktl::atomic<bool> done{false};
  ktl::atomic<zx_status_t> status{ZX_ERR_INTERNAL};
};

constexpr ktl::string_view kWriterMsg =
    "blocking write issued while the debug UART was suspended\n";

// Note that we must use `dlog_serial_write` (or its interruptible variant, the
// same path used by `zx_debug_write`) rather than calling
// `platform_dputs_thread` directly.  The UART driver does not support multiple
// concurrent blocking writers (only one waiter is woken per TX interrupt), so
// all blocking writers must be serialized with the debuglog thread via the
// debuglog serial lock.
int NonInterruptibleWriter(void* arg) {
  auto* state = static_cast<WriterState*>(arg);
  dlog_serial_write(kWriterMsg);
  state->status.store(ZX_OK, ktl::memory_order_relaxed);
  state->done.store(true, ktl::memory_order_relaxed);
  return 0;
}

int InterruptibleWriter(void* arg) {
  auto* state = static_cast<WriterState*>(arg);
  state->status.store(dlog_serial_write_interruptible(kWriterMsg), ktl::memory_order_relaxed);
  state->done.store(true, ktl::memory_order_relaxed);
  return 0;
}

// Verify that while the debug UART is suspended (a) blocking writers block
// until it's resumed, (b) non-blocking writers and polling reads do not hang,
// and (c) suspend/resume report BAD_STATE when called in the wrong state.
bool uart_suspend_resume_test() {
  BEGIN_TEST;

  zx_status_t status = platform_serial_suspend();
  if (status == ZX_ERR_NOT_SUPPORTED) {
    printf("debug UART suspend not supported, skipping\n");
    END_TEST;
  }
  ASSERT_OK(status);

  // No matter how the test exits, make sure the UART is resumed and that the
  // writer thread (if any) has terminated before |state| goes out of scope.
  WriterState state;
  Thread* writer = nullptr;
  auto cleanup = fit::defer([&writer]() {
    if (platform_serial_is_suspended()) {
      platform_serial_resume();
    }
    if (writer != nullptr) {
      writer->Join(nullptr, ZX_TIME_INFINITE);
    }
  });

  EXPECT_TRUE(platform_serial_is_suspended());
  EXPECT_EQ(ZX_ERR_BAD_STATE, platform_serial_suspend());

  // None of these may block or touch the hardware.
  serial_write("this non-blocking output should be dropped\n");
  platform_pputc('x');
  char c;
  EXPECT_EQ(-1, platform_pgetc(&c));

  // Start a thread to perform a blocking write.  It should not complete until
  // the UART has been resumed.
  writer = Thread::Create("uart suspend writer", NonInterruptibleWriter, &state, DEFAULT_PRIORITY);
  ASSERT_NONNULL(writer);
  writer->Resume();

  WaitUntilBlocked(writer);
  EXPECT_FALSE(state.done.load(ktl::memory_order_relaxed));

  ASSERT_OK(platform_serial_resume());
  EXPECT_FALSE(platform_serial_is_suspended());
  EXPECT_EQ(ZX_ERR_BAD_STATE, platform_serial_resume());

  ASSERT_OK(writer->Join(nullptr, ZX_TIME_INFINITE));
  writer = nullptr;
  EXPECT_TRUE(state.done.load(ktl::memory_order_relaxed));
  EXPECT_OK(state.status.load(ktl::memory_order_relaxed));

  END_TEST;
}

// Verify that a thread blocked in an interruptible write (as performed by
// `zx_debug_write`) waiting for the debug UART to be resumed can be killed.
bool uart_suspend_interruptible_write_kill_test() {
  BEGIN_TEST;

  zx_status_t status = platform_serial_suspend();
  if (status == ZX_ERR_NOT_SUPPORTED) {
    printf("debug UART suspend not supported, skipping\n");
    END_TEST;
  }
  ASSERT_OK(status);

  WriterState state;
  Thread* writer = nullptr;
  auto cleanup = fit::defer([&writer]() {
    if (writer != nullptr) {
      writer->Kill();
      writer->Join(nullptr, ZX_TIME_INFINITE);
    }
    if (platform_serial_is_suspended()) {
      platform_serial_resume();
    }
  });

  writer = Thread::Create("uart suspend interruptible writer", InterruptibleWriter, &state,
                          DEFAULT_PRIORITY);
  ASSERT_NONNULL(writer);
  writer->Resume();

  WaitUntilBlocked(writer);
  EXPECT_FALSE(state.done.load(ktl::memory_order_relaxed));

  // Kill the writer while the UART is still suspended.  It should terminate
  // without writing anything.
  writer->Kill();
  ASSERT_OK(writer->Join(nullptr, ZX_TIME_INFINITE));
  writer = nullptr;
  EXPECT_TRUE(state.done.load(ktl::memory_order_relaxed));
  EXPECT_EQ(ZX_ERR_INTERNAL_INTR_KILLED, state.status.load(ktl::memory_order_relaxed));
  EXPECT_TRUE(platform_serial_is_suspended());

  ASSERT_OK(platform_serial_resume());

  END_TEST;
}

// Verify that suspending a thread blocked in an interruptible write (as
// performed by `zx_debug_write`) interrupts the wait with
// ZX_ERR_INTERNAL_INTR_RETRY without writing anything.
bool uart_suspend_interruptible_write_thread_suspend_test() {
  BEGIN_TEST;

  zx_status_t status = platform_serial_suspend();
  if (status == ZX_ERR_NOT_SUPPORTED) {
    printf("debug UART suspend not supported, skipping\n");
    END_TEST;
  }
  ASSERT_OK(status);

  WriterState state;
  Thread* writer = nullptr;
  auto cleanup = fit::defer([&writer]() {
    if (platform_serial_is_suspended()) {
      platform_serial_resume();
    }
    if (writer != nullptr) {
      writer->Join(nullptr, ZX_TIME_INFINITE);
    }
  });

  writer = Thread::Create("uart suspend interruptible writer", InterruptibleWriter, &state,
                          DEFAULT_PRIORITY);
  ASSERT_NONNULL(writer);
  writer->Resume();

  WaitUntilBlocked(writer);
  EXPECT_FALSE(state.done.load(ktl::memory_order_relaxed));

  // Ask the writer to suspend while the UART is still suspended.  This should
  // interrupt the wait with ZX_ERR_INTERNAL_INTR_RETRY without writing
  // anything.
  ASSERT_OK(writer->Suspend());
  ASSERT_OK(writer->Join(nullptr, ZX_TIME_INFINITE));
  writer = nullptr;
  EXPECT_TRUE(state.done.load(ktl::memory_order_relaxed));
  EXPECT_EQ(ZX_ERR_INTERNAL_INTR_RETRY, state.status.load(ktl::memory_order_relaxed));
  EXPECT_TRUE(platform_serial_is_suspended());

  ASSERT_OK(platform_serial_resume());

  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(platform_debug_tests)
UNITTEST("suspend_resume", uart_suspend_resume_test)
UNITTEST("suspend_interruptible_write_kill", uart_suspend_interruptible_write_kill_test)
UNITTEST("suspend_interruptible_write_thread_suspend",
         uart_suspend_interruptible_write_thread_suspend_test)
UNITTEST_END_TESTCASE(platform_debug_tests, "platform_debug", "platform debug UART tests")
