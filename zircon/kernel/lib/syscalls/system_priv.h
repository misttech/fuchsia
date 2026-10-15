// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_SYSCALLS_SYSTEM_PRIV_H_
#define ZIRCON_KERNEL_LIB_SYSCALLS_SYSTEM_PRIV_H_

#include <zircon/syscalls-next.h>
#include <zircon/syscalls/system.h>

zx_status_t system_mexec_core(zx_handle_t resource, zx_handle_t kernel_vmo,
                              zx_handle_t data_zbi_vmo);

extern "C" {
zx_status_t cpp_system_mexec_payload_get_helper(uint8_t* buffer, size_t buffer_size,
                                                size_t* out_zbi_size);
zx_status_t cpp_system_mexec_core(zx_handle_t resource, zx_handle_t kernel_vmo,
                                  zx_handle_t data_zbi_vmo);
void cpp_scheduler_update_processing_rates(zx_cpu_performance_info_t* info, size_t count);
void cpp_scheduler_update_processing_limits(zx_cpu_perf_limit_t* info, size_t count);
void cpp_scheduler_get_performance_scales(zx_cpu_performance_info_t* info, size_t count);
void cpp_scheduler_get_default_performance_scales(zx_cpu_performance_info_t* info, size_t count);
void cpp_scheduler_get_processing_limits(zx_cpu_perf_limit_t* info, size_t count);
size_t cpp_percpu_processor_count();
}

#endif  // ZIRCON_KERNEL_LIB_SYSCALLS_SYSTEM_PRIV_H_
