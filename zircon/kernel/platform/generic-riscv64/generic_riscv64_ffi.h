// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PLATFORM_GENERIC_RISCV64_GENERIC_RISCV64_FFI_H_
#define ZIRCON_KERNEL_PLATFORM_GENERIC_RISCV64_GENERIC_RISCV64_FFI_H_

#include <lib/zbi-format/driver-config.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <dev/power.h>
#include <kernel/cpu.h>

__BEGIN_CDECLS

// The Rust platform's entry points, behind the C++-linkage declarations of
// <platform.h>, <platform/efi.h>, <platform/timer.h> and <dev/init.h>.

bool rust_is_efi_expected(void);

void rust_platform_early_init(void);
void rust_platform_prevm_init(void);
void rust_platform_init(void);
void rust_platform_panic_start(bool halt_other_cpus);
void rust_platform_halt_cpu(void) __NO_RETURN;
bool rust_platform_supports_suspend_cpu(void);
zx_status_t rust_platform_suspend_cpu(bool allow_domain_power_down);
bool rust_platform_supports_suspend_system(void);
zx_status_t rust_platform_suspend_system(void);
zx_status_t rust_platform_get_cpu_state(cpu_num_t cpu_id, power_cpu_state* out_state);
// `suggested_action` is a platform_halt_action and `reason` a zircon_crash_reason_t.
void rust_platform_specific_halt(uint32_t suggested_action, uint32_t reason,
                                 bool halt_on_panic) __NO_RETURN;

zx_instant_mono_ticks_t rust_platform_convert_early_ticks(uint64_t sample_time);
zx_status_t rust_platform_set_oneshot_timer(zx_ticks_t deadline);
void rust_platform_stop_timer(void);
void rust_platform_shutdown_timer(void);
zx_status_t rust_platform_suspend_timer_curr_cpu(void);
zx_status_t rust_platform_resume_timer_curr_cpu(void);
bool rust_platform_usermode_can_access_tick_registers(void);
// One entry point per GetTicksSyncFlag combination, N being the flag bits.
zx_ticks_t rust_platform_current_raw_ticks_synchronized_0(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_1(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_2(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_3(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_4(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_5(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_6(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_7(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_8(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_9(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_10(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_11(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_12(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_13(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_14(void);
zx_ticks_t rust_platform_current_raw_ticks_synchronized_15(void);

void rust_platform_driver_handoff_early(const zbi_dcfg_riscv_plic_driver_t* plic_driver,
                                        const zbi_dcfg_riscv_generic_timer_driver_t* timer_driver);
void rust_platform_driver_handoff_post_vm(const zbi_dcfg_riscv_plic_driver_t* plic_driver);
void rust_platform_driver_handoff_late(const zbi_dcfg_riscv_plic_driver_t* plic_driver);

// C++ helpers for the Rust platform.

// Binds the platform's MappedCrashlog to the `size` bytes of persistent RAM at
// `base`.
void cpp_mapped_crashlog_bind(void* base, size_t size);

__END_CDECLS

#endif  // ZIRCON_KERNEL_PLATFORM_GENERIC_RISCV64_GENERIC_RISCV64_FFI_H_
