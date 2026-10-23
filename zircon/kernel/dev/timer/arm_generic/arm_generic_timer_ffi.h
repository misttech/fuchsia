// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_TIMER_ARM_GENERIC_ARM_GENERIC_TIMER_FFI_H_
#define ZIRCON_KERNEL_DEV_TIMER_ARM_GENERIC_ARM_GENERIC_TIMER_FFI_H_

#include <lib/arch/ticks.h>
#include <lib/zbi-format/driver-config.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

__BEGIN_CDECLS

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
zx_ticks_t rust_platform_convert_early_ticks(arch::EarlyTicks sample);
zx_status_t rust_platform_set_oneshot_timer(zx_ticks_t deadline);
void rust_platform_stop_timer();
void rust_platform_shutdown_timer();
zx_status_t rust_platform_suspend_timer_curr_cpu();
zx_status_t rust_platform_resume_timer_curr_cpu();
bool rust_platform_usermode_can_access_tick_registers();
void rust_arm_generic_timer_init(const zbi_dcfg_arm_generic_timer_driver_t& config);
void rust_arm_generic_timer_init_post_vm(const zbi_dcfg_arm_generic_timer_driver_t& config);
bool rust_arm_use_phys_timer_in_vdso();

__END_CDECLS

#endif  // ZIRCON_KERNEL_DEV_TIMER_ARM_GENERIC_ARM_GENERIC_TIMER_FFI_H_
