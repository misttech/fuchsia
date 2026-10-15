// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fit/defer.h>
#include <lib/standalone-test/standalone.h>
#include <lib/zx/channel.h>
#include <lib/zx/event.h>
#include <lib/zx/job.h>
#include <lib/zx/process.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <lib/zx/suspend_token.h>
#include <lib/zx/thread.h>
#include <lib/zx/time.h>
#include <lib/zx/vmar.h>
#include <zircon/assert.h>
#include <zircon/status.h>
#include <zircon/syscalls-next.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>
#include <zircon/syscalls/resource.h>

#include <mini-process/mini-process.h>
#include <zxtest/zxtest.h>

#include "../needs-next.h"
#include "../threads/test-thread.h"
#include "../threads/thread-functions/thread-functions.h"

NEEDS_NEXT_SYSCALL(zx_debug_suspend);
NEEDS_NEXT_SYSCALL(zx_debug_resume);

namespace {

zx::resource GetSystemResourceWithBase(uint64_t base) {
  zx::unowned_resource system_resource = standalone::GetSystemResource();
  zx::result<zx::resource> result = standalone::GetSystemResourceWithBase(system_resource, base);
  ZX_ASSERT(result.is_ok());
  return std::move(result.value());
}

zx::resource GetDebugResource() { return GetSystemResourceWithBase(ZX_RSRC_SYSTEM_DEBUG_BASE); }

zx::resource GetCpuResource() { return GetSystemResourceWithBase(ZX_RSRC_SYSTEM_CPU_BASE); }

// Probes for debug serial port suspend support without suspending the port.
// Returns false if it is not supported.
//
// Also verifies that the port is not already suspended (e.g. left suspended by
// an earlier failing test), so that tests fail rather than block in
// zx_debug_write.  If it was, this resumes it.
bool DebugSuspendSupported(const zx::resource& debug_resource) {
  const zx_status_t status = zx_debug_resume(debug_resource.get());
  if (status == ZX_ERR_NOT_SUPPORTED) {
    return false;
  }
  EXPECT_STATUS(status, ZX_ERR_BAD_STATE, "the debug serial port was unexpectedly suspended");
  return true;
}

TEST(DebugSuspend, InvalidHandle) {
  NEEDS_NEXT_SKIP(zx_debug_suspend);
  NEEDS_NEXT_SKIP(zx_debug_resume);
  EXPECT_STATUS(zx_debug_suspend(ZX_HANDLE_INVALID), ZX_ERR_BAD_HANDLE);
  EXPECT_STATUS(zx_debug_resume(ZX_HANDLE_INVALID), ZX_ERR_BAD_HANDLE);
}

TEST(DebugSuspend, WrongHandleType) {
  NEEDS_NEXT_SKIP(zx_debug_suspend);
  NEEDS_NEXT_SKIP(zx_debug_resume);
  zx::event event;
  ASSERT_OK(zx::event::create(0, &event));
  EXPECT_STATUS(zx_debug_suspend(event.get()), ZX_ERR_WRONG_TYPE);
  EXPECT_STATUS(zx_debug_resume(event.get()), ZX_ERR_WRONG_TYPE);
}

TEST(DebugSuspend, WrongResource) {
  NEEDS_NEXT_SKIP(zx_debug_suspend);
  NEEDS_NEXT_SKIP(zx_debug_resume);
  zx::resource cpu_resource = GetCpuResource();
  EXPECT_STATUS(zx_debug_suspend(cpu_resource.get()), ZX_ERR_OUT_OF_RANGE);
  EXPECT_STATUS(zx_debug_resume(cpu_resource.get()), ZX_ERR_OUT_OF_RANGE);
}

TEST(DebugSuspend, SuspendResume) {
  NEEDS_NEXT_SKIP(zx_debug_suspend);
  NEEDS_NEXT_SKIP(zx_debug_resume);
  zx::resource debug_resource = GetDebugResource();

  // Not suspended, so resume should fail.
  zx_status_t status = zx_debug_resume(debug_resource.get());
  if (status == ZX_ERR_NOT_SUPPORTED) {
    ZXTEST_SKIP("debug serial port suspend is not supported on this system");
  }
  EXPECT_STATUS(status, ZX_ERR_BAD_STATE);

  ASSERT_OK(zx_debug_suspend(debug_resource.get()));
  auto cleanup = fit::defer([&]() { zx_debug_resume(debug_resource.get()); });

  // Already suspended.
  EXPECT_STATUS(zx_debug_suspend(debug_resource.get()), ZX_ERR_BAD_STATE);

  // Logging while suspended should not hang anything.  This output will be
  // queued in the debuglog and emitted once we resume.  There is nothing to
  // assert here; the test would hang (and time out) if it did.
  printf("This message was logged while the debug serial port was suspended.\n");

  cleanup.cancel();
  EXPECT_OK(zx_debug_resume(debug_resource.get()));

  // No longer suspended.
  EXPECT_STATUS(zx_debug_resume(debug_resource.get()), ZX_ERR_BAD_STATE);

  // We should be able to do it all again.
  ASSERT_OK(zx_debug_suspend(debug_resource.get()));
  EXPECT_OK(zx_debug_resume(debug_resource.get()));
}

constexpr char kDebugWriteMsg[] =
    "zx_debug_write issued while the debug serial port was suspended\n";

// There is no reliable way to observe that a thread is blocked in
// zx_debug_write (it is reported as ZX_THREAD_STATE_RUNNING), so tests give
// threads this long to block.  If a thread has not yet blocked by the time a
// test acts on it, the test may pass without testing anything, but it will
// never fail spuriously.
constexpr zx::duration kBlockDelay = zx::msec(100);

// Suspends the debug serial port for the duration of a test, and starts a
// thread which calls zx_debug_write() (and will therefore block until the port
// is resumed).
//
// Tests should call Setup() via ASSERT_NO_FATAL_FAILURE and return early if
// skipped() is true.
//
// No matter how the test exits, the destructor resumes the serial port before
// the writer thread is waited upon (by |writer_|'s destructor), so a failing
// test cannot hang or leave the serial port suspended.
class SuspendedDebugWriteTest {
 public:
  void Setup() {
    debug_resource_ = GetDebugResource();
    if (!DebugSuspendSupported(debug_resource_)) {
      skipped_ = true;
      return;
    }

    // zx_debug_write requires kernel.enable-serial-syscalls.  Probe for it.
    // The port is not suspended, so this does not block.
    if (zx_debug_write("", 0) == ZX_ERR_NOT_SUPPORTED) {
      skipped_ = true;
      return;
    }

    ASSERT_OK(zx_debug_suspend(debug_resource_.get()));
    suspended_ = true;

    ASSERT_NO_FATAL_FAILURE(writer_.Init("debug-write"));
    ASSERT_NO_FATAL_FAILURE(writer_.Start(threads_test_debug_write_fn, &arg_));

    // Give the writer time to block in zx_debug_write.  It should remain there
    // for as long as the debug serial port is suspended.
    ASSERT_FALSE(WriterTerminatesWithin(kBlockDelay));
  }

  ~SuspendedDebugWriteTest() { Resume(); }

  bool skipped() const { return skipped_; }

  void Resume() {
    if (suspended_) {
      suspended_ = false;
      EXPECT_OK(zx_debug_resume(debug_resource_.get()));
    }
  }

  // Returns true if the writer thread terminates within |timeout|.
  bool WriterTerminatesWithin(zx::duration timeout) const {
    return writer_.thread().wait_one(ZX_THREAD_TERMINATED, zx::deadline_after(timeout), nullptr) ==
           ZX_OK;
  }

  bool writer_terminated() const { return WriterTerminatesWithin(zx::duration(0)); }

  const zx::thread& writer_thread() const { return writer_.thread(); }
  void WaitForWriter() { writer_.Wait(); }
  zx_status_t writer_status() const { return arg_.status; }

 private:
  // A value which zx_debug_write will never return.
  static constexpr zx_status_t kNotSet = ZX_ERR_INTERNAL;

  // |arg_| is used by the writer thread, so it must outlive |writer_|, whose
  // destructor waits for the thread to terminate.
  debug_write_arg arg_{kDebugWriteMsg, sizeof(kDebugWriteMsg) - 1, kNotSet};
  TestThread writer_;
  zx::resource debug_resource_;
  bool skipped_ = false;
  bool suspended_ = false;
};

TEST(DebugSuspend, DebugWriteBlocksWhileSuspended) {
  NEEDS_NEXT_SKIP(zx_debug_suspend);
  NEEDS_NEXT_SKIP(zx_debug_resume);
  SuspendedDebugWriteTest test;
  ASSERT_NO_FATAL_FAILURE(test.Setup());
  if (test.skipped()) {
    ZXTEST_SKIP("zx_debug_write or debug serial port suspend is not supported on this system");
  }

  // The writer is blocked (see Setup) and should remain so for as long as the
  // debug serial port is suspended.
  EXPECT_FALSE(test.WriterTerminatesWithin(kBlockDelay));

  test.Resume();
  ASSERT_NO_FATAL_FAILURE(test.WaitForWriter());
  EXPECT_OK(test.writer_status());
}

// Verify that a thread blocked in zx_debug_write() while the debug serial port
// is suspended can be suspended and resumed, and that once the debug serial
// port is resumed the call completes with ZX_OK.
TEST(DebugSuspend, DebugWriteSuspendableWhileBlocked) {
  NEEDS_NEXT_SKIP(zx_debug_suspend);
  NEEDS_NEXT_SKIP(zx_debug_resume);
  SuspendedDebugWriteTest test;
  ASSERT_NO_FATAL_FAILURE(test.Setup());
  if (test.skipped()) {
    ZXTEST_SKIP("zx_debug_write or debug serial port suspend is not supported on this system");
  }

  zx::suspend_token token;
  ASSERT_OK(test.writer_thread().suspend(&token));

  // The writer should suspend while the debug serial port is still suspended.
  ASSERT_OK(test.writer_thread().wait_one(ZX_THREAD_SUSPENDED, zx::time::infinite(), nullptr));
  zx_info_thread_t info;
  ASSERT_OK(test.writer_thread().get_info(ZX_INFO_THREAD, &info, sizeof(info), nullptr, nullptr));
  EXPECT_EQ(ZX_THREAD_STATE_SUSPENDED, info.state);
  EXPECT_FALSE(test.writer_terminated());

  // Resume the writer while the debug serial port is still suspended.  Because
  // zx_debug_write is @blocking, the vDSO should restart the call and the
  // writer should block again until the debug serial port is resumed.
  token.reset();
  EXPECT_FALSE(test.WriterTerminatesWithin(kBlockDelay));

  // Suspend the writer a second time, then resume the debug serial port before
  // resuming the writer.
  ASSERT_OK(test.writer_thread().suspend(&token));
  ASSERT_OK(test.writer_thread().wait_one(ZX_THREAD_SUSPENDED, zx::time::infinite(), nullptr));
  test.Resume();
  token.reset();

  ASSERT_NO_FATAL_FAILURE(test.WaitForWriter());
  EXPECT_OK(test.writer_status());
}

// Verify that a process with a thread blocked in zx_debug_write() can be
// killed.  (Individual threads cannot be killed, so we use a separate
// mini-process.)
TEST(DebugSuspend, DebugWriteKillableWhileBlocked) {
  NEEDS_NEXT_SKIP(zx_debug_suspend);
  NEEDS_NEXT_SKIP(zx_debug_resume);
  zx::resource debug_resource = GetDebugResource();
  if (!DebugSuspendSupported(debug_resource)) {
    ZXTEST_SKIP("debug serial port suspend is not supported on this system");
  }

  // zx_debug_write requires kernel.enable-serial-syscalls.  Probe for it.  The
  // port is not suspended, so this does not block.
  if (zx_debug_write("", 0) == ZX_ERR_NOT_SUPPORTED) {
    ZXTEST_SKIP("zx_debug_write is not enabled on this system");
  }

  ASSERT_OK(zx_debug_suspend(debug_resource.get()));
  auto cleanup = fit::defer([&]() { EXPECT_OK(zx_debug_resume(debug_resource.get())); });

  constexpr char kName[] = "debug-write-kill";
  zx::process process;
  zx::vmar vmar;
  ASSERT_OK(
      zx::process::create(*zx::job::default_job(), kName, sizeof(kName) - 1, 0, &process, &vmar));
  // Make sure the process is dead before the test exits, no matter how.
  auto kill_process = fit::defer([&process]() {
    process.kill();
    process.wait_one(ZX_TASK_TERMINATED, zx::time::infinite(), nullptr);
  });
  zx::thread thread;
  ASSERT_OK(zx::thread::create(process, kName, sizeof(kName) - 1, 0, &thread));
  zx::channel control;
  ASSERT_OK(start_mini_process_etc(process.get(), thread.get(), vmar.get(), ZX_HANDLE_INVALID, true,
                                   control.reset_and_get_address()));

  // Ask the mini-process to call zx_debug_write.  It should block for as long
  // as the debug serial port is suspended, so it should not reply.  Give it
  // time to block before killing it.
  ASSERT_OK(mini_process_cmd_send(control.get(), MINIP_CMD_DEBUG_WRITE));
  EXPECT_STATUS(control.wait_one(ZX_CHANNEL_READABLE, zx::deadline_after(kBlockDelay), nullptr),
                ZX_ERR_TIMED_OUT);

  // Kill the process while the debug serial port is still suspended.  It should
  // terminate without ever replying.
  ASSERT_OK(process.kill());
  ASSERT_OK(process.wait_one(ZX_TASK_TERMINATED, zx::time::infinite(), nullptr));
  EXPECT_STATUS(control.wait_one(ZX_CHANNEL_READABLE, zx::time::infinite_past(), nullptr),
                ZX_ERR_TIMED_OUT);
}

// Verify that a process with a thread blocked in zx_debug_read() does not
// prevent the debug serial port from being suspended, remains blocked while the
// port is suspended (since no new input can arrive), and can be killed while
// the port is suspended.  (Individual threads cannot be killed, so we use a
// separate mini-process.)
TEST(DebugSuspend, DebugReadBlocksWhileSuspended) {
  NEEDS_NEXT_SKIP(zx_debug_suspend);
  NEEDS_NEXT_SKIP(zx_debug_resume);
  zx::resource debug_resource = GetDebugResource();

  if (!DebugSuspendSupported(debug_resource)) {
    ZXTEST_SKIP("debug serial port suspend is not supported on this system");
  }

  constexpr char kName[] = "debug-read-suspended";
  zx::process process;
  zx::vmar vmar;
  ASSERT_OK(
      zx::process::create(*zx::job::default_job(), kName, sizeof(kName) - 1, 0, &process, &vmar));
  // Make sure the process is dead before the test exits, no matter how.
  auto kill_process = fit::defer([&process]() {
    process.kill();
    process.wait_one(ZX_TASK_TERMINATED, zx::time::infinite(), nullptr);
  });
  zx::thread thread;
  ASSERT_OK(zx::thread::create(process, kName, sizeof(kName) - 1, 0, &thread));
  zx::channel control;
  ASSERT_OK(start_mini_process_etc(process.get(), thread.get(), vmar.get(), ZX_HANDLE_INVALID, true,
                                   control.reset_and_get_address()));

  // Returns true if the mini-process replies before |deadline|, which it does
  // only once zx_debug_read has failed.
  auto replied = [&control](zx::time deadline) {
    return control.wait_one(ZX_CHANNEL_READABLE, deadline, nullptr) == ZX_OK;
  };
  // Returns true if the mini-process has terminated (e.g. crashed).  Used to
  // distinguish a reader which is still blocked from one which died.
  auto terminated = [&process]() {
    return process.wait_one(ZX_TASK_TERMINATED, zx::time::infinite_past(), nullptr) == ZX_OK;
  };

  // Ask the mini-process to read (consuming any buffered input) until it
  // blocks in zx_debug_read.  If zx_debug_read is not enabled on this system,
  // it replies immediately with ZX_ERR_NOT_SUPPORTED.  The handle is consumed
  // even on failure.
  zx::resource read_resource;
  ASSERT_OK(debug_resource.duplicate(ZX_RIGHT_SAME_RIGHTS, &read_resource));
  ASSERT_OK(mini_process_cmd_send(control.get(), MINIP_CMD_DEBUG_READ, read_resource.release()));
  if (replied(zx::deadline_after(kBlockDelay))) {
    zx_status_t cmd_status = mini_process_cmd_read_reply(control.get(), nullptr);
    if (cmd_status == ZX_ERR_NOT_SUPPORTED) {
      ZXTEST_SKIP("zx_debug_read is not enabled on this system");
    }
    FAIL("MINIP_CMD_DEBUG_READ completed unexpectedly: %s", zx_status_get_string(cmd_status));
  }
  ASSERT_FALSE(terminated(), "the reader terminated unexpectedly");

  // A blocked reader must not prevent the port from being suspended.
  ASSERT_OK(zx_debug_suspend(debug_resource.get()));
  auto resume = fit::defer([&]() { EXPECT_OK(zx_debug_resume(debug_resource.get())); });

  // The reader should remain blocked while the port is suspended.
  EXPECT_FALSE(replied(zx::deadline_after(kBlockDelay)));
  EXPECT_FALSE(terminated(), "the reader terminated unexpectedly");

  // Kill the process while the port is still suspended.  It should terminate
  // without ever replying.
  kill_process.cancel();
  ASSERT_OK(process.kill());
  ASSERT_OK(process.wait_one(ZX_TASK_TERMINATED, zx::time::infinite(), nullptr));
  EXPECT_FALSE(replied(zx::time::infinite_past()));
}

}  // namespace
