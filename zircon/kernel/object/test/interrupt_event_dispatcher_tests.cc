// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/errors.h>

#include <platform/timer.h>
#if ARCH_X86
#include <arch/x86/apic.h>
#endif
#include <lib/fit/defer.h>
#include <lib/unittest/unittest.h>
#include <platform.h>
#include <zircon/syscalls-next.h>

#include <kernel/idle_power_thread.h>
#include <ktl/atomic.h>
#include <object/interrupt_dispatcher.h>
#include <object/interrupt_event_dispatcher.h>

#include <ktl/enforce.h>

namespace {

bool TestPendingWakeEventBlocksSuspend() {
  BEGIN_TEST;

  // Generating the interrupt events for this test is necessarily arch specific and is only
  // implemented for x86 here.
#if ARCH_X86
  KernelHandle<InterruptDispatcher> interrupt;
  zx_rights_t rights;

  uint32_t gsi;
  constexpr uint32_t gsi_search_max = 24;
  for (gsi = 0; gsi < gsi_search_max; gsi++) {
    zx_status_t status =
        InterruptEventDispatcher::Create(&interrupt, &rights, gsi, ZX_INTERRUPT_WAKE_VECTOR,
                                         /*allow_ack_without_port_for_test=*/true);
    if (status == ZX_OK) {
      break;
    }
  }
  ASSERT_NE(gsi, gsi_search_max, "Failed to find free global interrupt");

  // Move to the boot CPU to guarantee there are no races between handling the interrupt and calling
  // suspend.
  auto restore_affinity = fit::defer(
      [previous_affinity = Thread::Current::Get()->SetCpuAffinity(cpu_num_to_mask(BOOT_CPU_ID))] {
        Thread::Current::Get()->SetCpuAffinity(previous_affinity);
      });
  ASSERT_EQ(arch_curr_cpu_num(), static_cast<cpu_num_t>(BOOT_CPU_ID));

  ASSERT_EQ(0u, IdlePowerThread::pending_wake_events());

  // Make sure suspend entry works before testing suspend abort due to pending wake events.
  zx_instant_boot_t resume_at = current_boot_time() + ZX_SEC(5);
  IdlePowerThread::TransitionAllActiveToSuspend(resume_at);

  // Pend a wake event by issuing the IPI.
  const uint8_t vector = apic_io_fetch_irq_vector(gsi);
  apic_send_self_ipi(vector, DELIVERY_MODE_FIXED);
  ASSERT_EQ(1u, IdlePowerThread::pending_wake_events());

  // Suspend entry should immediately bounce out with pending wake events.
  resume_at = current_boot_time() + ZX_SEC(5);
  IdlePowerThread::TransitionAllActiveToSuspend(resume_at);
  ASSERT_EQ(1u, IdlePowerThread::pending_wake_events());

  // Wait for the interrupt to advance the state machine from TRIGGERED to NEEDACK.
  zx_instant_boot_t timestamp = 0;
  zx_status_t status = interrupt.dispatcher()->WaitForInterrupt(&timestamp);
  ASSERT_EQ(ZX_OK, status);
  ASSERT_EQ(1u, IdlePowerThread::pending_wake_events());

  // Acknowledge the wake event to allow suspend again.
  status = interrupt.dispatcher()->Ack();
  ASSERT_EQ(ZX_OK, status);
  ASSERT_EQ(0u, IdlePowerThread::pending_wake_events());

  // Suspend entry should succeed again.
  resume_at = current_boot_time() + ZX_SEC(5);
  IdlePowerThread::TransitionAllActiveToSuspend(resume_at);

  // Check that a pending wake event is acknowledged if the interrupt event dispatcher is destroyed
  // without an explicit acknowledgment.
  apic_send_self_ipi(vector, DELIVERY_MODE_FIXED);
  ASSERT_EQ(1u, IdlePowerThread::pending_wake_events());

  // Suspend entry should immediately bounce out with pending wake events.
  resume_at = current_boot_time() + ZX_SEC(5);
  IdlePowerThread::TransitionAllActiveToSuspend(resume_at);

  interrupt.reset();
  ASSERT_EQ(0u, IdlePowerThread::pending_wake_events());

  // Suspend entry should succeed.
  resume_at = current_boot_time() + ZX_SEC(5);
  IdlePowerThread::TransitionAllActiveToSuspend(resume_at);
#endif

  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(interrupt_event_dispatcher_tests)
UNITTEST("PendingWakeEventBlocksSuspend", TestPendingWakeEventBlocksSuspend)
UNITTEST_END_TESTCASE(interrupt_event_dispatcher_tests, "interrupt_event_dispatcher_tests",
                      "InterruptEventDispatcher tests")
