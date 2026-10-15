// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZIRCON_SYSCALLS_SYSTEM_H_
#define ZIRCON_SYSCALLS_SYSTEM_H_

#include <zircon/types.h>

__BEGIN_CDECLS

// Commands used by zx_system_powerctl()
#define ZX_SYSTEM_POWERCTL_REBOOT 5u
#define ZX_SYSTEM_POWERCTL_REBOOT_BOOTLOADER 6u
#define ZX_SYSTEM_POWERCTL_REBOOT_RECOVERY 7u
#define ZX_SYSTEM_POWERCTL_SHUTDOWN 8u
#define ZX_SYSTEM_POWERCTL_ACK_KERNEL_INITIATED_REBOOT 9u

// Topics used by zx_system_{get,set}_performance_info():
#define ZX_CPU_PERF_SCALE ((uint32_t)1u)
#define ZX_CPU_DEFAULT_PERF_SCALE ((uint32_t)2u)
#define ZX_CPU_PERF_LIMIT ((uint32_t)3u)

typedef struct zx_cpu_performance_scale {
  uint32_t integral_part;
  uint32_t fractional_part;
} zx_cpu_performance_scale_t;

typedef struct zx_cpu_performance_info {
  uint32_t logical_cpu_number;
  zx_cpu_performance_scale_t performance_scale;
} zx_cpu_performance_info_t;

typedef uint32_t zx_cpu_perf_limit_type_t;

// Represents a limit on the processing rate of the CPU. Values for min and
// max are interpreted to be in the range [0, MAX_RATE], where MAX_RATE is
// the value of the processing_rate field of the fastest power level in the
// same power domain. Values above MAX_RATE are clamped to MAX_RATE.
#define ZX_CPU_PERF_LIMIT_TYPE_RATE ((zx_cpu_perf_limit_type_t)(0u))

// Represents a limit on the effective power of the CPU. Values for min and
// max are in units of nanowatts.
#define ZX_CPU_PERF_LIMIT_TYPE_POWER ((zx_cpu_perf_limit_type_t)(1u))

typedef struct {
  uint32_t logical_cpu_number;
  zx_cpu_perf_limit_type_t limit_type;
  uint64_t min;
  uint64_t max;
} zx_cpu_perf_limit_t;

__END_CDECLS

#endif  // ZIRCON_SYSCALLS_SYSTEM_H_
