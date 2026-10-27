// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2009 Corey Tabaka
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/ticks.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <platform/pc/timer.h>
#include <platform/timer.h>

#include <ktl/enforce.h>

static_assert(sizeof(arch::EarlyTicks) == 8 && alignof(arch::EarlyTicks) == 8);

// TODO(https://fxbug.dev/576428709): Remove these C++ forwarding shims once all callers are
// migrated to Rust.
extern "C" {

zx_ticks_t rust_platform_current_raw_ticks_synchronized_0();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_1();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_2();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_3();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_4();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_5();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_6();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_7();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_8();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_9();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_10();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_11();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_12();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_13();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_14();
zx_ticks_t rust_platform_current_raw_ticks_synchronized_15();

zx_duration_t rust_convert_raw_tsc_duration_to_nanoseconds(int64_t duration);
zx_instant_mono_t rust_convert_raw_tsc_timestamp_to_clock_monotonic(int64_t ts);
zx_status_t rust_platform_set_oneshot_timer(zx_ticks_t deadline);
void rust_platform_stop_timer();
void rust_platform_shutdown_timer();
zx_status_t rust_platform_suspend_timer_curr_cpu();
zx_status_t rust_platform_resume_timer_curr_cpu();
zx_instant_mono_ticks_t rust_platform_convert_early_ticks(arch::EarlyTicks sample);
bool rust_platform_usermode_can_access_tick_registers();

}  // extern "C"

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

zx_duration_t convert_raw_tsc_duration_to_nanoseconds(int64_t duration) {
  return rust_convert_raw_tsc_duration_to_nanoseconds(duration);
}

zx_instant_mono_t convert_raw_tsc_timestamp_to_clock_monotonic(int64_t ts) {
  return rust_convert_raw_tsc_timestamp_to_clock_monotonic(ts);
}

zx_status_t platform_set_oneshot_timer(zx_ticks_t deadline) {
  return rust_platform_set_oneshot_timer(deadline);
}

void platform_stop_timer() { rust_platform_stop_timer(); }

void platform_shutdown_timer() { rust_platform_shutdown_timer(); }

zx_status_t platform_suspend_timer_curr_cpu() { return rust_platform_suspend_timer_curr_cpu(); }

zx_status_t platform_resume_timer_curr_cpu() { return rust_platform_resume_timer_curr_cpu(); }

zx_instant_mono_ticks_t platform_convert_early_ticks(arch::EarlyTicks sample) {
  return rust_platform_convert_early_ticks(sample);
}

bool platform_usermode_can_access_tick_registers() {
  return rust_platform_usermode_can_access_tick_registers();
}
