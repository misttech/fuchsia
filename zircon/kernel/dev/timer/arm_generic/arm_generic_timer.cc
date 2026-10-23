// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/ticks.h>
#include <lib/zbi-format/driver-config.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <dev/timer/arm_generic.h>
#include <platform/timer.h>

#include "arm_generic_timer_ffi.h"

static_assert(sizeof(arch::EarlyTicks) == 16 && alignof(arch::EarlyTicks) == 8);
static_assert(sizeof(zbi_dcfg_arm_generic_timer_driver_t) == 16 &&
              alignof(zbi_dcfg_arm_generic_timer_driver_t) == 4);

template <GetTicksSyncFlag Flags>
inline zx_ticks_t platform_current_raw_ticks_synchronized() {
  switch (static_cast<uint8_t>(Flags)) {
    case 0:
      return rust_platform_current_raw_ticks_synchronized_0();
    case 1:
      return rust_platform_current_raw_ticks_synchronized_1();
    case 2:
      return rust_platform_current_raw_ticks_synchronized_2();
    case 3:
      return rust_platform_current_raw_ticks_synchronized_3();
    case 4:
      return rust_platform_current_raw_ticks_synchronized_4();
    case 5:
      return rust_platform_current_raw_ticks_synchronized_5();
    case 6:
      return rust_platform_current_raw_ticks_synchronized_6();
    case 7:
      return rust_platform_current_raw_ticks_synchronized_7();
    case 8:
      return rust_platform_current_raw_ticks_synchronized_8();
    case 9:
      return rust_platform_current_raw_ticks_synchronized_9();
    case 10:
      return rust_platform_current_raw_ticks_synchronized_10();
    case 11:
      return rust_platform_current_raw_ticks_synchronized_11();
    case 12:
      return rust_platform_current_raw_ticks_synchronized_12();
    case 13:
      return rust_platform_current_raw_ticks_synchronized_13();
    case 14:
      return rust_platform_current_raw_ticks_synchronized_14();
    case 15:
      return rust_platform_current_raw_ticks_synchronized_15();
    default:
      __UNREACHABLE;
  }
}

// Explicit instantiation of all of the forms of synchronized tick access.
#define EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(flags) \
  template zx_ticks_t                                         \
  platform_current_raw_ticks_synchronized<static_cast<GetTicksSyncFlag>(flags)>()
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(0);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(1);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(2);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(3);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(4);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(5);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(6);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(7);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(8);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(9);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(10);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(11);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(12);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(13);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(14);
EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED(15);
#undef EXPAND_PLATFORM_CURRENT_RAW_TICKS_SYNCHRONIZED

zx_ticks_t platform_convert_early_ticks(arch::EarlyTicks sample) {
  return rust_platform_convert_early_ticks(sample);
}

zx_status_t platform_set_oneshot_timer(zx_ticks_t deadline) {
  return rust_platform_set_oneshot_timer(deadline);
}

void platform_stop_timer() { rust_platform_stop_timer(); }

void platform_shutdown_timer() { rust_platform_shutdown_timer(); }

zx_status_t platform_suspend_timer_curr_cpu() { return rust_platform_suspend_timer_curr_cpu(); }

zx_status_t platform_resume_timer_curr_cpu() { return rust_platform_resume_timer_curr_cpu(); }

bool platform_usermode_can_access_tick_registers() {
  return rust_platform_usermode_can_access_tick_registers();
}

void ArmGenericTimerInit(const zbi_dcfg_arm_generic_timer_driver_t& config) {
  rust_arm_generic_timer_init(config);
}

void ArmGenericTimerInitPostVm(const zbi_dcfg_arm_generic_timer_driver_t& config) {
  rust_arm_generic_timer_init_post_vm(config);
}

bool ArmUsePhysTimerInVdso() { return rust_arm_use_phys_timer_in_vdso(); }
