// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/unittest/unittest.h>
#include <platform.h>
#include <zircon/errors.h>
#include <zircon/time.h>

#include <kernel/deadline.h>
#include <kernel/idle_power_thread.h>
#include <kernel/mp.h>
#include <kernel/percpu.h>
#include <kernel/scheduler.h>
#include <kernel/thread.h>

#if defined(__aarch64__)
#include <arch/arm64/feature.h>
#endif

// Provides tests with access to IdlePowerThread internals.
struct IdlePowerThreadTestAccess {
  // Calls SuspendAllActive in System mode, even if the platform does not support
  // platform_suspend_system (unlike SuspendSystem, which would use Cpu mode).
  static zx_instant_boot_t SuspendAllActiveSystemMode(zx_instant_boot_t resume_at) {
    return IdlePowerThread::SuspendAllActive(resume_at, IdlePowerThread::SuspendMode::System);
  }
};

namespace {

// Exercises the System mode of SuspendAllActive (used by SuspendSystem) regardless of whether the
// platform supports platform_suspend_system.  The non-boot CPUs are taken offline (waiting for the
// platform to confirm that they have powered off), the boot CPU suspends the system (or, if system
// suspend is not supported, falls back to suspending just itself) until the resume timer fires, and
// then the non-boot CPUs are brought back online.  Verifies that every CPU which was online and
// active beforehand is online and active again afterwards.
bool SuspendAllActiveSystemModeRestoresCpus() {
  BEGIN_TEST;

#if defined(__riscv)
  // TODO(https://fxbug.dev/42086046): Re-enable when RISC-V supports CPU hotplug.
  printf("skipping test, hotplug only supported on x64 and arm64\n");
  END_TEST;
#endif

  // System mode requires that the platform be able to confirm that CPUs have powered off.
  if (platform_get_cpu_state(BOOT_CPU_ID).status_value() == ZX_ERR_NOT_SUPPORTED) {
    printf("platform_get_cpu_state is not supported, skipping test\n");
    END_TEST;
  }

  const cpu_mask_t online_before = mp_get_online_mask();
  const cpu_mask_t active_before = Scheduler::PeekActiveMask();

  IdlePowerThreadTestAccess::SuspendAllActiveSystemMode(
      zx_time_add_duration(current_boot_time(), ZX_MSEC(100)));

  // SuspendAllActive waits for each CPU's idle/power thread to become active again, but the online
  // and scheduler active masks may be updated slightly later, so allow some time for them to catch
  // up.
  const Deadline deadline = Deadline::after_mono(ZX_SEC(10));
  while ((mp_get_online_mask() != online_before || Scheduler::PeekActiveMask() != active_before) &&
         current_mono_time() < deadline.when()) {
    Thread::Current::SleepRelative(ZX_USEC(200));
  }
  EXPECT_EQ(online_before, mp_get_online_mask());
  EXPECT_EQ(active_before, Scheduler::PeekActiveMask());

#if defined(__aarch64__)
  // Kernel code runs with PSTATE.PAN set (when FEAT_PAN is implemented), but PAN is not preserved
  // across a CPU powerdown, so make sure it was restored on every CPU that may have powered down.
  // PSTATE.PAN is not saved and restored per-thread, so if a CPU had resumed with PAN clear, the
  // threads it switches to would observe it clear as well.  Run on each CPU and check.
  if (arm64_mmu_features.pan) {
    const cpu_mask_t previous_affinity = Thread::Current::Get()->GetCpuAffinity();
    for (cpu_num_t cpu = 0; cpu < percpu::processor_count(); ++cpu) {
      if ((active_before & cpu_num_to_mask(cpu)) == 0) {
        continue;
      }
      Thread::Current::SetCpuAffinity(cpu_num_to_mask(cpu));
      const uint64_t pan = __arm_rsr64("s3_0_c4_c2_3");
      if (pan == 0) {
        printf("PSTATE.PAN is clear on CPU %u\n", cpu);
      }
      EXPECT_NE(0u, pan);
    }
    Thread::Current::SetCpuAffinity(previous_affinity);
  }
#endif

  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(idle_power_thread_tests)
UNITTEST("suspend_all_active_system_mode_restores_cpus", SuspendAllActiveSystemModeRestoresCpus)
UNITTEST_END_TESTCASE(idle_power_thread_tests, "idle_power_thread", "IdlePowerThread tests")
