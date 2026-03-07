// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fit/defer.h>
#include <lib/zx/bti.h>
#include <lib/zx/event.h>
#include <lib/zx/process.h>
#include <lib/zx/thread.h>
#include <lib/zx/vmar.h>
#include <lib/zx/vmo.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/syscalls-next.h>
#include <zircon/syscalls/rseq.h>

#include <atomic>
#include <thread>

#include <zxtest/zxtest.h>

#include "../needs-next.h"

NEEDS_NEXT_SYSCALL(zx_thread_set_rseq);

// Executes a restartable sequence that only terminates if:
//   - a CPU migration is observed (bad).  Returns 0.
//   - Zircon aborts the sequence (good).  Returns 1.
//
// See rseq-arm64.S, rseq-riscv64.S, rseq-x64.S.
extern "C" int SpinUntilMigratedOrAborted(zx_rseq_t* rseq);

namespace {

constexpr uint32_t kInvalidCpuId = 0xffff'ffff;

// Verify that an interrupted restartable sequence is aborted.
TEST(RseqTest, Abort) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  // Create and map a VMO to back the zx_rseq_t.
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));
  zx_vaddr_t addr;
  ASSERT_OK(zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo, 0,
                                       sizeof(zx_rseq_t), &addr));
  auto unmap =
      fit::defer([addr]() { ASSERT_OK(zx::vmar::root_self()->unmap(addr, sizeof(zx_rseq_t))); });
  auto* rseq = reinterpret_cast<zx_rseq_t*>(addr);

  // Register the rseq struct.  SpinUntilMigratedOrAborted will populate the fields.
  ASSERT_OK(zx_thread_set_rseq(vmo.get(), 0, sizeof(zx_rseq_t)));
  auto cleanup = fit::defer([]() { ASSERT_OK(zx_thread_set_rseq(ZX_HANDLE_INVALID, 0, 0)); });

  std::atomic<bool> is_spinning{false};
  std::atomic<bool> is_done{false};

  // Get a handle to the current thread so we can suspend it later.
  zx::thread spinning_thread;
  ASSERT_OK(zx::thread::self()->duplicate(ZX_RIGHT_SAME_RIGHTS, &spinning_thread));

  zx_status_t helper_status = ZX_ERR_INTERNAL;

  // Spawn a helper thread to suspend/resume the rseq thread once it has started spinning.
  std::thread helper([&spinning_thread, &is_spinning, &is_done, &helper_status]() {
    // Wait for the main thread to start spinning.
    while (!is_spinning.load()) {
      std::this_thread::yield();
    }

    while (!is_done.load()) {
      // Suspend the main thread.
      zx::suspend_token token;
      helper_status = spinning_thread.suspend(&token);
      if (helper_status != ZX_OK) {
        return;
      }

      // Wait for the main thread to actually suspend.
      zx_signals_t observed;
      helper_status =
          spinning_thread.wait_one(ZX_THREAD_SUSPENDED, zx::time::infinite(), &observed);
      if (helper_status != ZX_OK) {
        return;
      }

      // Resume the main thread.
      token.reset();

      // Sleep briefly before the next "kick".
      zx::nanosleep(zx::deadline_after(zx::msec(10)));
    }

    helper_status = ZX_OK;
  });

  auto join_helper = fit::defer([&helper, &helper_status]() {
    helper.join();
    ASSERT_OK(helper_status);
  });

  // Execute a restartable sequence that will spin until:
  //   * The sequence is aborted by Zircon.
  //   * The sequence observes (via zx_rseq_t::cpu_id) a thread migration has occurred.
  is_spinning.store(true);
  int result = SpinUntilMigratedOrAborted(rseq);
  is_done.store(true);

  ASSERT_TRUE(result);
}

// Verifies that various invalid sequences do not trigger a kernel panic.
TEST(RseqTest, InvalidRseq) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  // Create and map a VMO to back the zx_rseq_t.
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));
  zx_vaddr_t addr;
  ASSERT_OK(zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo, 0,
                                       sizeof(zx_rseq_t), &addr));
  auto unmap =
      fit::defer([addr]() { ASSERT_OK(zx::vmar::root_self()->unmap(addr, sizeof(zx_rseq_t))); });
  auto* rseq = reinterpret_cast<zx_rseq_t*>(addr);
  ASSERT_EQ(rseq->start_ip, 0);
  ASSERT_EQ(rseq->post_commit_offset, 0);
  ASSERT_EQ(rseq->abort_ip, 0);
  rseq->cpu_id = kInvalidCpuId;

  // Register the rseq struct.
  ASSERT_OK(zx_thread_set_rseq(vmo.get(), 0, sizeof(zx_rseq_t)));
  auto cleanup = fit::defer([]() { ASSERT_OK(zx_thread_set_rseq(ZX_HANDLE_INVALID, 0, 0)); });
  ASSERT_NE(kInvalidCpuId, std::atomic_ref(rseq->cpu_id).load());

  // Get start and end of root VMAR so we can define a sequence that encompasses the entire program.
  zx_info_vmar_t vmar;
  ASSERT_OK(zx::vmar::root_self()->get_info(ZX_INFO_VMAR, &vmar, sizeof(vmar), nullptr, nullptr));

  auto TestWith = [&rseq](zx_vaddr_t start_ip, size_t post_commit_offset, zx_vaddr_t abort_ip) {
    // Prepare.
    std::atomic_ref(rseq->start_ip).store(start_ip);
    std::atomic_ref(rseq->abort_ip).store(abort_ip);

    // Activate.
    std::atomic_ref(rseq->post_commit_offset).store(post_commit_offset);

    // Sleep until we know a check has been performed.
    std::atomic_ref(rseq->cpu_id).store(kInvalidCpuId);
    do {
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    } while (std::atomic_ref(rseq->cpu_id).load() == kInvalidCpuId);

    // Deactivate.
    std::atomic_ref(rseq->post_commit_offset).store(0);
  };

  // 0 is a valid abort_ip in the sense that the kernel will happily branch us there when requested.
  // See that we don't crash because our ip will never be within in the range of the sequence.
  ASSERT_NO_FATAL_FAILURE(TestWith(0xffff'ffff'ffff'ffff, 0, 0));
  ASSERT_NO_FATAL_FAILURE(TestWith(0xffff'ffff'ffff'ffff, 1, 0));
  ASSERT_NO_FATAL_FAILURE(TestWith(0xffff'ffff'ffff'ffff, 2, 0));

#if defined(__x86_64__)
  // Validate that on x86, the kernel filters or ignores non-userspace abort_ip values.  On other
  // architectures no filtering is performed so the result is an exception generated by the calling
  // thread.
  //
  // TODO(https://fxbug.dev/490516066): Extend this test to verify that arm64 and riscv64 generate
  // an exception.
  ASSERT_NO_FATAL_FAILURE(TestWith(vmar.base, vmar.len, 0xffff'ffff'ffff'ffff));
  ASSERT_NO_FATAL_FAILURE(TestWith(vmar.base, vmar.len, 0xffff'ffff'ffff'0000));
  ASSERT_NO_FATAL_FAILURE(TestWith(vmar.base, vmar.len, 0xffff'0000'000'0000));
  ASSERT_NO_FATAL_FAILURE(TestWith(vmar.base, vmar.len, 0x000f'0000'000'0000));
#endif
}

// Verify the happy case for register and unregister operations.
TEST(RseqTest, RegisterUnregister) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(sizeof(zx_rseq_t), 0, &vmo));

  // Register.
  ASSERT_OK(zx_thread_set_rseq(vmo.get(), 0, sizeof(zx_rseq_t)));

  // Unregister.
  ASSERT_OK(zx_thread_set_rseq(ZX_HANDLE_INVALID, 0, 0));
}

TEST(RseqTest, InvalidUnregisterArgs) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  ASSERT_EQ(zx_thread_set_rseq(ZX_HANDLE_INVALID, sizeof(zx_rseq_t), 0), ZX_ERR_INVALID_ARGS);
  ASSERT_EQ(zx_thread_set_rseq(ZX_HANDLE_INVALID, 0, sizeof(zx_rseq_t)), ZX_ERR_INVALID_ARGS);
  ASSERT_EQ(zx_thread_set_rseq(ZX_HANDLE_INVALID, sizeof(zx_rseq_t), sizeof(zx_rseq_t)),
            ZX_ERR_INVALID_ARGS);
}

TEST(RseqTest, UnregisterNotRegistered) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  // Unregistering when not registered should be fine.
  ASSERT_OK(zx_thread_set_rseq(ZX_HANDLE_INVALID, 0, 0));
}

TEST(RseqTest, InvalidSize) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(sizeof(zx_rseq_t) * 2, 0, &vmo));
  ASSERT_EQ(zx_thread_set_rseq(vmo.get(), 0, sizeof(zx_rseq_t) - 1), ZX_ERR_INVALID_ARGS);
  ASSERT_EQ(zx_thread_set_rseq(vmo.get(), 0, sizeof(zx_rseq_t) + 1), ZX_ERR_INVALID_ARGS);
  ASSERT_EQ(zx_thread_set_rseq(vmo.get(), 0, 0), ZX_ERR_INVALID_ARGS);
}

TEST(RseqTest, InvalidOffset) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(sizeof(zx_rseq_t) * 2, 0, &vmo));

  // Offset must be aligned.
  ASSERT_EQ(zx_thread_set_rseq(vmo.get(), 1, sizeof(zx_rseq_t)), ZX_ERR_INVALID_ARGS);

  // The zx_rseq_t must not span page boundaries.
  ASSERT_EQ(zx_thread_set_rseq(vmo.get(), zx_system_get_page_size() - 1, sizeof(zx_rseq_t)),
            ZX_ERR_INVALID_ARGS);
}

TEST(RseqTest, ArithmeticOverflow) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(sizeof(zx_rseq_t), 0, &vmo));
  // Test with a huge offset that would overflow if added to size.
  ASSERT_EQ(zx_thread_set_rseq(vmo.get(), UINT64_MAX - sizeof(zx_rseq_t) + 1, sizeof(zx_rseq_t)),
            ZX_ERR_OUT_OF_RANGE);
}

TEST(RseqTest, BadHandle) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  // 0xbadc0ffe is always a bad handle value since bit 0 of a valid handle value must always be 1.
  ASSERT_EQ(zx_thread_set_rseq(0xbadc0ffe, 0, sizeof(zx_rseq_t)), ZX_ERR_BAD_HANDLE);
}

TEST(RseqTest, InsufficientRights) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(sizeof(zx_rseq_t), 0, &vmo));

  zx_info_handle_basic_t info;
  ASSERT_OK(vmo.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr));
  const zx_rights_t rights = info.rights;

  zx::vmo no_read;
  ASSERT_OK(vmo.duplicate(rights & ~ZX_RIGHT_READ, &no_read));
  ASSERT_EQ(zx_thread_set_rseq(no_read.get(), 0, sizeof(zx_rseq_t)), ZX_ERR_ACCESS_DENIED);

  zx::vmo no_write;
  ASSERT_OK(vmo.duplicate(rights & ~ZX_RIGHT_WRITE, &no_write));
  ASSERT_EQ(zx_thread_set_rseq(no_write.get(), 0, sizeof(zx_rseq_t)), ZX_ERR_ACCESS_DENIED);

  zx::vmo no_dup;
  ASSERT_OK(vmo.duplicate(rights & ~ZX_RIGHT_DUPLICATE, &no_dup));
  ASSERT_EQ(zx_thread_set_rseq(no_dup.get(), 0, sizeof(zx_rseq_t)), ZX_ERR_ACCESS_DENIED);
}

TEST(RseqTest, WrongHandleType) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  zx::event event;
  ASSERT_OK(zx::event::create(0, &event));
  ASSERT_EQ(zx_thread_set_rseq(event.get(), 0, sizeof(zx_rseq_t)), ZX_ERR_WRONG_TYPE);
}

TEST(RseqTest, DoubleRegister) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(sizeof(zx_rseq_t), 0, &vmo));
  zx::vmo vmo2;
  ASSERT_OK(zx::vmo::create(sizeof(zx_rseq_t), 0, &vmo2));

  ASSERT_OK(zx_thread_set_rseq(vmo.get(), 0, sizeof(zx_rseq_t)));
  auto cleanup = fit::defer([]() { ASSERT_OK(zx_thread_set_rseq(ZX_HANDLE_INVALID, 0, 0)); });

  // Second call should also succeed, replacing the first registration.
  ASSERT_OK(zx_thread_set_rseq(vmo2.get(), 0, sizeof(zx_rseq_t)));
}

TEST(RseqTest, UpdateCpuId) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));

  zx_vaddr_t addr;
  ASSERT_OK(zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo, 0,
                                       sizeof(zx_rseq_t), &addr));
  auto unmap =
      fit::defer([addr]() { ASSERT_OK(zx::vmar::root_self()->unmap(addr, sizeof(zx_rseq_t))); });

  zx_rseq_t* rseq = reinterpret_cast<zx_rseq_t*>(addr);
  // Initialize with an invalid CPU ID.
  rseq->cpu_id = kInvalidCpuId;

  ASSERT_OK(zx_thread_set_rseq(vmo.get(), 0, sizeof(zx_rseq_t)));
  auto cleanup = fit::defer([]() { ASSERT_OK(zx_thread_set_rseq(ZX_HANDLE_INVALID, 0, 0)); });

  // After registration, the kernel should have updated the cpu_id to a valid value.
  ASSERT_NE(kInvalidCpuId, rseq->cpu_id);
}

}  // namespace
