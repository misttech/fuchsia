// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <inttypes.h>
#include <lib/fit/defer.h>
#include <lib/unittest/unittest.h>
#include <platform.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/interrupt.h>
#include <dev/power.h>
#include <kernel/cpu.h>
#include <kernel/dpc.h>
#include <kernel/event.h>
#include <kernel/mp.h>
#include <kernel/scheduler.h>
#include <kernel/thread.h>
#include <ktl/atomic.h>
#include <ktl/iterator.h>

#include "tests.h"

#include <ktl/enforce.h>

static int resume_cpu_test_thread(void* arg) {
  *reinterpret_cast<cpu_num_t*>(arg) = arch_curr_cpu_num();
  return 0;
}

// "Unplug" online secondary (non-BOOT) cores
static zx_status_t unplug_all_cores() {
  cpu_mask_t cpumask = mp_get_online_mask() & ~cpu_num_to_mask(BOOT_CPU_ID);
  return mp_unplug_cpu_mask(cpumask, ZX_TIME_INFINITE);
}

static zx_status_t hotplug_core(cpu_num_t i) {
  cpu_mask_t cpumask = cpu_num_to_mask(i);
  return mp_hotplug_cpu_mask(cpumask);
}

static unsigned get_num_cpus_online() {
  unsigned count = 0;
  cpu_mask_t online = mp_get_online_mask();
  while (online) {
    online >>= 1;
    ++count;
  }
  return count;
}

static zx_status_t wait_for_cpu_offline(cpu_num_t i) {
  zx_instant_mono_t print_time = zx_time_add_duration(current_mono_time(), ZX_SEC(5));
  while (true) {
    zx::result<power_cpu_state> res = platform_get_cpu_state(i);
    if (res.is_error()) {
      if (res.error_value() == ZX_ERR_NOT_SUPPORTED) {
        // x86 does not implement platform_get_cpu_state, so return OK if the call returns
        // ZX_ERR_NOT_SUPPORTED.
        return ZX_OK;
      }
      return res.error_value();
    } else if (res.value() == power_cpu_state::OFF || res.value() == power_cpu_state::STOPPED) {
      return ZX_OK;
    }
    if (current_mono_time() > print_time) {
      print_time = zx_time_add_duration(current_mono_time(), ZX_SEC(5));
      printf("Still waiting for CPU %u to go offline, waiting 5 more seconds\n", i);
    }
    Thread::Current::SleepRelative(ZX_USEC(200));
  }
}

static void wait_for_cpu_active(cpu_num_t i) {
  zx_instant_mono_t print_time = zx_time_add_duration(current_mono_time(), ZX_SEC(5));
  while (true) {
    if (Scheduler::PeekIsActive(i)) {
      return;
    }
    if (current_mono_time() > print_time) {
      print_time = zx_time_add_duration(current_mono_time(), ZX_SEC(5));
      printf("Still waiting for CPU %u to become active, waiting 5 more seconds\n", i);
    }
    Thread::Current::SleepRelative(ZX_USEC(200));
  }
}

// Wait until |i|'s DpcRunner is running, by queuing a Dpc on each of |i|'s Dpc queues and waiting
// for them to execute.
//
// A hotplugged core becomes active (see wait_for_cpu_active) before it has created its Dpc threads,
// and a core must not be unplugged before then (mp_unplug_cpu_mask shuts down its Dpc threads).
// The general queue's thread is created (and may start executing Dpcs) before the low latency
// queue's thread, so both queues must be checked.
static zx_status_t wait_for_cpu_dpc_runner(cpu_num_t i) {
  Thread* t = Thread::Create(
      "wait-for-dpc-runner",
      [](void*) -> int {
        constexpr DpcRunner::QueueType kQueueTypes[] = {DpcRunner::QueueType::General,
                                                        DpcRunner::QueueType::LowLatency};
        for (const DpcRunner::QueueType type : kQueueTypes) {
          Event event;
          Dpc dpc([](Dpc* d) { d->arg<Event>()->Signal(); }, &event);
          const zx_status_t status = DpcRunner::Enqueue(dpc, type);
          if (status != ZX_OK) {
            return status;
          }
          // Wait non-interruptibly, since |dpc| and |event| live on this thread's stack.
          event.Wait();
        }
        return ZX_OK;
      },
      nullptr, DEFAULT_PRIORITY);
  if (t == nullptr) {
    return ZX_ERR_NO_MEMORY;
  }
  t->SetCpuAffinity(cpu_num_to_mask(i));
  t->Resume();
  int retcode;
  const zx_status_t status = t->Join(&retcode, ZX_TIME_INFINITE);
  return status != ZX_OK ? status : retcode;
}

// Unplug all cores (except for Boot core), then hotplug
// the cores one by one and make sure that we can schedule
// tasks on that core.
[[maybe_unused]] static bool mp_hotplug_test() {
  BEGIN_TEST;

// TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
#if defined(__riscv)
  printf("skipping test mp_hotplug, hotplug only suported on x64 and arm64\n");
  END_TEST;
#endif

  uint num_cores = get_num_cpus_online();
  if (num_cores < 2) {
    printf("skipping test mp_hotplug, not enough online cpus\n");
    END_TEST;
  }
  Thread::Current::MigrateToCpu(BOOT_CPU_ID);
  // "Unplug" online secondary (non-BOOT) cores
  ASSERT_OK(unplug_all_cores(), "unplugging all cores failed");
  for (cpu_num_t i = 0; i < num_cores; i++) {
    if (i == BOOT_CPU_ID) {
      continue;
    }
    // Wait until this core is fully offline.
    ASSERT_OK(wait_for_cpu_offline(i), "waiting for core to go offline failed");
    // Hotplug this core.
    ASSERT_OK(hotplug_core(i), "hotplugging core failed");
    // Wait until the core is active.
    wait_for_cpu_active(i);
    // Create a thread, affine it to the core just hotplugged
    // and make sure the thread does get scheduled there.
    cpu_num_t running_core{INVALID_CPU};
    Thread* nt = Thread::Create("resume-test-thread", resume_cpu_test_thread, &running_core,
                                DEFAULT_PRIORITY);
    ASSERT_NE(nullptr, nt, "Thread create failed");
    nt->SetCpuAffinity(cpu_num_to_mask(i));
    nt->SetMigrateFn([](auto...) {});
    nt->Resume();
    ASSERT_OK(nt->Join(nullptr, ZX_TIME_INFINITE), "thread join failed");
    ASSERT_EQ(i, running_core, "Thread not running on hotplugged core");
  }

  END_TEST;
}

// Repeatedly unplug and hotplug a core while another thread repeatedly calls mp_sync_exec targeting
// it, and make sure that every mp_sync_exec call returns.
//
// A task may be queued for the core after it has disabled interrupts to go offline, in which case
// the task is drained when the core is marked offline rather than executed.  Otherwise, the task is
// executed by the core before it goes offline.  Either way, mp_sync_exec must not wait
// indefinitely for the core, and must not return while its task is still queued (which
// mp_sync_exec checks with a DEBUG_ASSERT).
[[maybe_unused]] static bool mp_hotplug_sync_exec_test() {
  BEGIN_TEST;

// TODO(https://fxbug.dev/42086046): Re-enable test on RISC-V.
#if defined(__riscv)
  printf("skipping test mp_hotplug_sync_exec, hotplug only supported on x64 and arm64\n");
  END_TEST;
#endif

  const cpu_mask_t online_secondaries = mp_get_online_mask() & ~cpu_num_to_mask(BOOT_CPU_ID);
  if (online_secondaries == 0) {
    printf("skipping test mp_hotplug_sync_exec, not enough online cpus\n");
    END_TEST;
  }
  const cpu_num_t target_cpu = highest_cpu_set(online_secondaries);

  auto restore_affinity = fit::defer(
      [previous_affinity = Thread::Current::SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID))] {
        Thread::Current::SetCpuAffinity(previous_affinity);
      });

  // If an assertion below fails while |target_cpu| is unplugged, hotplug it again so that
  // subsequent tests run with the same set of cores.
  auto restore_target_cpu = fit::defer([target_cpu] {
    if (!mp_is_cpu_online(target_cpu)) {
      const zx_status_t status = hotplug_core(target_cpu);
      ASSERT_MSG(status == ZX_OK, "hotplugging core %u failed: %d", target_cpu, status);
      wait_for_cpu_active(target_cpu);
      const zx_status_t dpc_status = wait_for_cpu_dpc_runner(target_cpu);
      ASSERT_MSG(dpc_status == ZX_OK, "waiting for core %u's dpc runner failed: %d", target_cpu,
                 dpc_status);
    }
  });

  struct State {
    const cpu_num_t target_cpu;
    ktl::atomic<bool> stop{false};
    ktl::atomic<uint64_t> calls{0};
  } state{.target_cpu = target_cpu};

  // The sender may run on any online core other than the one being unplugged and hotplugged.
  Thread* sender = Thread::Create(
      "mp-hotplug-sync-exec-sender",
      [](void* arg) -> int {
        auto* s = static_cast<State*>(arg);
        while (!s->stop.load(ktl::memory_order_relaxed)) {
          // Alternate between targeting just the core being unplugged and hotplugged, and all
          // other cores.
          const uint64_t calls = s->calls.load(ktl::memory_order_relaxed);
          if (calls % 2 == 0) {
            mp_sync_exec(
                mp_ipi_target::MASK, cpu_num_to_mask(s->target_cpu), [](void*) {}, nullptr);
          } else {
            // Targeting all but the local core requires interrupts to be disabled.
            InterruptDisableGuard irqd;
            mp_sync_exec(mp_ipi_target::ALL_BUT_LOCAL, 0, [](void*) {}, nullptr);
          }
          s->calls.store(calls + 1, ktl::memory_order_relaxed);
          // While the target core is offline, every other call returns immediately, so yield to
          // avoid monopolizing this core.
          Thread::Current::Yield();
        }
        return 0;
      },
      &state, DEFAULT_PRIORITY);
  ASSERT_NONNULL(sender, "Thread create failed");
  sender->SetCpuAffinity(mp_get_online_mask() & ~cpu_num_to_mask(target_cpu));
  sender->Resume();

  // Stop the sender, and wait for it to finish, before |state| goes out of scope, even if an
  // assertion below fails.  If any mp_sync_exec call fails to return, the sender never observes
  // |stop|, and this test hangs.
  auto stop_sender = fit::defer([&state, sender] {
    state.stop.store(true, ktl::memory_order_relaxed);
    sender->Join(nullptr, ZX_TIME_INFINITE);
  });

  constexpr int kIterations = 20;
  for (int i = 0; i < kIterations; ++i) {
    ASSERT_OK(mp_unplug_cpu_mask(cpu_num_to_mask(target_cpu), ZX_TIME_INFINITE),
              "unplugging core failed");
    ASSERT_OK(wait_for_cpu_offline(target_cpu), "waiting for core to go offline failed");
    ASSERT_OK(hotplug_core(target_cpu), "hotplugging core failed");
    wait_for_cpu_active(target_cpu);
    ASSERT_OK(wait_for_cpu_dpc_runner(target_cpu), "waiting for core's dpc runner failed");
  }

  stop_sender.call();
  EXPECT_GT(state.calls.load(ktl::memory_order_relaxed), 0u);

  END_TEST;
}

UNITTEST_START_TESTCASE(mp_hotplug_tests)
UNITTEST("test unplug and hotplug cores one by one", mp_hotplug_test)
UNITTEST("test mp_sync_exec while unplugging and hotplugging a core", mp_hotplug_sync_exec_test)
UNITTEST_END_TESTCASE(mp_hotplug_tests, "hotplug", "Tests for unplugging and hotplugging cores")
