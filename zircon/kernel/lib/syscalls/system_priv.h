// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_SYSCALLS_SYSTEM_PRIV_H_
#define ZIRCON_KERNEL_LIB_SYSCALLS_SYSTEM_PRIV_H_

#include <zircon/syscalls-next.h>
#include <zircon/syscalls/system.h>

extern "C" {
void cpp_scheduler_update_processing_rates(zx_cpu_performance_info_t* info, size_t count);
void cpp_scheduler_update_processing_limits(zx_cpu_perf_limit_t* info, size_t count);
void cpp_scheduler_get_performance_scales(zx_cpu_performance_info_t* info, size_t count);
void cpp_scheduler_get_default_performance_scales(zx_cpu_performance_info_t* info, size_t count);
void cpp_scheduler_get_processing_limits(zx_cpu_perf_limit_t* info, size_t count);
size_t cpp_percpu_processor_count();

#if defined(__x86_64__)
bool cpp_x86_power_limit_suported();
void cpp_x86_status_callback_toggle();
void cpp_x86_print_limit_reasons(bool use_log);
void cpp_x86_clear_limit_reason_log();
void cpp_x86_print_limits();
#endif
}

#endif  // ZIRCON_KERNEL_LIB_SYSCALLS_SYSTEM_PRIV_H_
