// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <inttypes.h>
#include <lib/unittest/unittest.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <kernel/auto_preempt_disabler.h>
#include <kernel/cpu.h>
#include <kernel/mp.h>
#include <kernel/thread.h>

#include "tests.h"

#include <ktl/enforce.h>

namespace {

// Verify that mp_reschedule_self() ensures a preemption is pending (almost) immediately after
// re-enabling interrupts.
bool mp_reschedule_self_test() {
  BEGIN_TEST;

  while (true) {
    AutoPreemptDisabler apd;
    const cpu_mask_t local_mask = cpu_num_to_mask(arch_curr_cpu_num());
    {
      InterruptDisableGuard irqd;

      // Now that interrupts are disabled, make sure that we don't already have a preemption
      // pending.  It's possible that another CPU in the system is trying to preempt this CPU.  If
      // that has happened, just try again.
      if (local_mask & Thread::Current::preemption_state().preempts_pending()) {
        continue;
      }

      mp_reschedule_self();
      // We've just IPI'd our CPU.  However, interrupts are disabled so that IPI won't have resulted
      // in a pending preemption just yet.
      ASSERT_EQ(0u, local_mask & Thread::Current::preemption_state().preempts_pending());
    }
    // We've just now re-enabled interrupts.  Either the IPI should have fired or it should fire
    // soon.
    zx_instant_mono_t last = current_mono_time();
    while ((local_mask & Thread::Current::preemption_state().preempts_pending()) == 0) {
      arch::Yield();
      zx_instant_mono_t now = current_mono_time();
      if (now > last + ZX_SEC(5)) {
        printf("still waiting for preemption...\n");
        last = now;
      }
    }

    break;
  }

  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(mp_tests)
UNITTEST("mp_reschedule_self", mp_reschedule_self_test)
UNITTEST_END_TESTCASE(mp_tests, "mp", "tests for mp subsystem")
