// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_THREAD_SAMPLER_INCLUDE_LIB_THREAD_SAMPLER_THREAD_SAMPLER_FFI_H_
#define ZIRCON_KERNEL_LIB_THREAD_SAMPLER_INCLUDE_LIB_THREAD_SAMPLER_THREAD_SAMPLER_FFI_H_

#include <arch.h>
#include <stdint.h>
#include <zircon/types.h>

extern "C" {

void rust_sampler_percpu_init();
void rust_sampler_percpu_shutdown();
void rust_thread_sampler_reschedule_marking();
zx_status_t rust_thread_sampler_sample_thread(zx_koid_t pid, zx_koid_t tid,
                                              GeneralRegsSource source, const void* gregs,
                                              uint64_t session_id);

}  // extern "C"

#endif  // ZIRCON_KERNEL_LIB_THREAD_SAMPLER_INCLUDE_LIB_THREAD_SAMPLER_THREAD_SAMPLER_FFI_H_
