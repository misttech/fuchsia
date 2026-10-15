// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <lib/fit/defer.h>
#include <lib/unittest/unittest.h>
#include <platform.h>
#include <zircon/errors.h>

#include <arch/interrupt.h>
#include <kernel/auto_preempt_disabler.h>
#include <kernel/deadline.h>
#include <kernel/mp.h>
#include <kernel/scheduler.h>

namespace {

// Runs |fn| on the boot CPU with all secondary CPUs unplugged and restores the
// secondary CPUs and thread affinity before returning.
template <typename F>
bool run_with_secondaries_unplugged(F fn) {
  BEGIN_TEST;

#if defined(__riscv)
  // TODO(https://fxbug.dev/42086046): Re-enable when RISC-V supports CPU hotplug.
  printf("skipping test, hotplug only supported on x64 and arm64\n");
  END_TEST;
#endif

  auto restore_affinity = fit::defer(
      [previous_affinity = Thread::Current::Get()->SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID))]() {
        Thread::Current::Get()->SetCpuAffinity(previous_affinity);
      });
  ASSERT_EQ(static_cast<cpu_num_t>(BOOT_CPU_ID), arch_curr_cpu_num());

  const cpu_mask_t online_before = mp_get_online_mask();
  auto restore_cpus = fit::defer([online_before]() {
    const cpu_mask_t to_hotplug = online_before & ~mp_get_online_mask();
    if (to_hotplug != 0) {
      const zx_status_t status = mp_hotplug_cpu_mask(to_hotplug);
      ASSERT_MSG(status == ZX_OK, "mp_hotplug_cpu_mask(%#x) failed: %d", to_hotplug, status);
      // Bound the wait so that a CPU which fails to come back produces a clear failure rather than
      // a hang.  Subsequent tests could not run correctly without it anyway.
      const Deadline deadline = Deadline::after_mono(ZX_SEC(10));
      while ((mp_get_online_mask() & to_hotplug) != to_hotplug ||
             (Scheduler::PeekActiveMask() & to_hotplug) != to_hotplug) {
        ASSERT_MSG(current_mono_time() < deadline.when(),
                   "timed out waiting for CPUs %#x to come online (online %#x, active %#x)",
                   to_hotplug, mp_get_online_mask(), Scheduler::PeekActiveMask());
        Thread::Current::SleepRelative(ZX_USEC(200));
      }
    }
  });

  const cpu_mask_t secondaries = online_before & ~cpu_num_to_mask(BOOT_CPU_ID);
  if (secondaries != 0) {
    ASSERT_OK(mp_unplug_cpu_mask(secondaries, ZX_TIME_INFINITE));
  }
  ASSERT_EQ(cpu_num_to_mask(BOOT_CPU_ID), mp_get_online_mask());

  ASSERT_TRUE(fn());

  END_TEST;
}

bool cpu_suspend_not_supported() {
  BEGIN_TEST;

  if (platform_supports_suspend_cpu()) {
    printf("platform_suspend_cpu is supported, skipping test\n");
    END_TEST;
  }

  {
    AutoPreemptDisabler apd;
    InterruptDisableGuard irqd;
    ASSERT_EQ(ZX_ERR_NOT_SUPPORTED, platform_suspend_cpu(PlatformAllowDomainPowerDown::No));
  }

  END_TEST;
}

bool cpu_suspend_pending_ipi() {
  BEGIN_TEST;

  if (!platform_supports_suspend_cpu()) {
    printf("platform_suspend_cpu is not supported, skipping test\n");
    END_TEST;
  }

  {
    AutoPreemptDisabler apd;
    InterruptDisableGuard irqd;
    const cpu_num_t curr_cpu = arch_curr_cpu_num();

    // Pend an interrupt to ourselves to ensure that platform_suspend_cpu returns quickly.
    mp_interrupt(mp_ipi_target::MASK, cpu_num_to_mask(curr_cpu));

    const zx_status_t status = platform_suspend_cpu(PlatformAllowDomainPowerDown::No);
    ASSERT_OK(status);
  }

  END_TEST;
}

// This test creates two threads, A and B.  Each on their own CPU.  A will suspend and wait to be
// woken by B.  B will wait to observe that A has suspended before waking it via IPI.  Because A
// might wake from suspend due to a stray interrupt, we retry the test until it succeeds, an error
// occurs or the timeout has elapsed.
bool cpu_suspend_one_wakes_another() {
  BEGIN_TEST;

  constexpr zx_duration_boot_t kTimeout = ZX_SEC(10);

  if (!platform_supports_suspend_cpu()) {
    printf("platform_suspend_cpu is not supported, skipping test\n");
    END_TEST;
  }

  cpu_mask_t active_cpus = Scheduler::PeekActiveMask();
  if (ktl::popcount(active_cpus) <= 1) {
    printf("not enough active CPUs, skipping test\n");
    END_TEST;
  }

  enum class State : uint32_t {
    READY = 0,
    SUSPENDING,
    DONE,
  };
  struct Arg {
    const cpu_num_t target;
    ktl::atomic<State> target_state;
  };

  thread_start_routine a_entry = [](void* arg_) -> int {
    auto* arg = reinterpret_cast<Arg*>(arg_);

    AutoPreemptDisabler apd;
    InterruptDisableGuard irqd;

    arg->target_state.store(State::SUSPENDING, ktl::memory_order_release);
    const zx_status_t status = platform_suspend_cpu(PlatformAllowDomainPowerDown::No);
    arg->target_state.store(State::DONE, ktl::memory_order_release);

    return status;
  };

  thread_start_routine b_entry = [](void* arg_) -> int {
    auto* arg = reinterpret_cast<Arg*>(arg_);

    // Wait until the target reports that it's about to suspend.
    State reported;
    while ((reported = arg->target_state.load(ktl::memory_order_acquire)) == State::READY) {
      arch::Yield();
    }

    if (reported == State::SUSPENDING) {
      // Wait a short amount of time, see that the thread is still suspended, then wake it.
      Thread::Current::SleepRelative(ZX_USEC(200));
      if (arg->target_state.load(ktl::memory_order_acquire) == State::SUSPENDING) {
        mp_interrupt(mp_ipi_target::MASK, cpu_num_to_mask(arg->target));
        return ZX_OK;
      }
    }

    // Somehow the CPU must have returned from suspend.
    return ZX_ERR_BAD_STATE;
  };

  // First, select the CPU that will wake the other.  Choose the boot CPU because we know it's must
  // be active and because we want the suspending CPU to be one of the secondaries since they
  // receive fewer (if any) hardware interrupts (SPIs).
  ASSERT_TRUE(active_cpus & cpu_num_to_mask(BOOT_CPU_ID));
  const cpu_num_t cpu_b = BOOT_CPU_ID;
  active_cpus &= ~cpu_num_to_mask(BOOT_CPU_ID);
  const cpu_num_t cpu_a = remove_cpu_from_mask(active_cpus);

  const auto deadline = Deadline::after_boot(kTimeout);
  for (size_t attempt = 0; current_boot_time() < deadline.when(); ++attempt) {
    printf("attempt %zu\n", attempt);

    Arg arg(cpu_a, State::READY);

    auto start_thread = [&](const char* name, thread_start_routine entry, cpu_num_t cpu,
                            Thread*& out_thread) {
      out_thread = Thread::Create(name, entry, &arg, DEFAULT_PRIORITY);
      if (out_thread != nullptr) {
        out_thread->SetCpuAffinity(cpu_num_to_mask(cpu));
        out_thread->Resume();
      }
      return fit::defer([&out_thread]() {
        if (out_thread != nullptr) {
          out_thread->Join(nullptr, ZX_TIME_INFINITE);
        }
      });
    };

    Thread* a = nullptr;
    auto cleanup_a = start_thread("one_wakes_another-a", a_entry, cpu_a, a);
    ASSERT_NONNULL(a);

    Thread* b = nullptr;
    auto cleanup_b = start_thread("one_wakes_another-b", b_entry, cpu_b, b);
    ASSERT_NONNULL(b);

    zx_status_t a_status;
    a->Join(&a_status, ZX_TIME_INFINITE);
    cleanup_a.cancel();
    ASSERT_OK(a_status);

    zx_status_t b_status;
    b->Join(&b_status, ZX_TIME_INFINITE);
    cleanup_b.cancel();

    // Perhaps CPU-A woke from suspend early.  Try again.
    if (b_status == ZX_ERR_BAD_STATE) {
      continue;
    }

    ASSERT_OK(b_status);
    END_TEST;
  }

  printf("failed to complete successfully before timeout\n");
  ASSERT_TRUE(false);

  END_TEST;
}

// Verify that when platform_supports_suspend_system() returns false, platform_suspend_system()
// does not attempt to suspend and fails with ZX_ERR_NOT_SUPPORTED.  Secondary CPUs are unplugged so
// that all of platform_suspend_system()'s preconditions are met.
bool system_suspend_not_supported() {
  BEGIN_TEST;

  if (platform_supports_suspend_system()) {
    printf("platform_suspend_system is supported, skipping test\n");
    END_TEST;
  }

  ASSERT_TRUE(run_with_secondaries_unplugged([]() -> bool {
    BEGIN_TEST;
    AutoPreemptDisabler apd;
    InterruptDisableGuard irqd;
    ASSERT_EQ(ZX_ERR_NOT_SUPPORTED, platform_suspend_system());
    END_TEST;
  }));

  END_TEST;
}

bool system_suspend_pending_ipi() {
  BEGIN_TEST;

  if (!platform_supports_suspend_system()) {
    printf("platform_suspend_system is not supported, skipping test\n");
    END_TEST;
  }

  ASSERT_TRUE(run_with_secondaries_unplugged([]() -> bool {
    BEGIN_TEST;
    AutoPreemptDisabler apd;
    InterruptDisableGuard irqd;
    const cpu_num_t curr_cpu = arch_curr_cpu_num();
    ASSERT_EQ(static_cast<cpu_num_t>(BOOT_CPU_ID), curr_cpu);

    // Pend an interrupt to ourselves to ensure that platform_suspend_system returns quickly.
    mp_interrupt(mp_ipi_target::MASK, cpu_num_to_mask(curr_cpu));

    const zx_status_t status = platform_suspend_system();
    ASSERT_OK(status);
    END_TEST;
  }));

  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(platform_suspend_tests)
UNITTEST("cpu_suspend_not_supported", cpu_suspend_not_supported)
UNITTEST("cpu_suspend_pending_ipi", cpu_suspend_pending_ipi)
UNITTEST("cpu_suspend_one_wakes_another", cpu_suspend_one_wakes_another)
UNITTEST("system_suspend_not_supported", system_suspend_not_supported)
UNITTEST("system_suspend_pending_ipi", system_suspend_pending_ipi)
UNITTEST_END_TESTCASE(platform_suspend_tests, "platform_suspend_tests", "platform suspend tests")
